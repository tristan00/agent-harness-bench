//! Process ownership and bounded JSONL transport. This is supervision, not a security sandbox.
use crate::{Result, model::Manifest};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

pub const MOVE_TIMEOUT: Duration = Duration::from_millis(100);
pub const START_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_LINE: usize = 65536;
const MAX_LOG: usize = 65536;

pub struct Tree {
    #[cfg(windows)]
    handle: usize,
    #[cfg(unix)]
    pid: u32,
}
impl Tree {
    pub fn stop(&self) {
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.handle as _, 1);
        }
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.pid as i32), libc::SIGKILL);
        }
    }
    pub fn usage(&self) -> (u64, u64) {
        #[cfg(windows)]
        unsafe {
            use windows_sys::Win32::System::JobObjects::*;
            let mut memory: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            let mut cpu: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
            QueryInformationJobObject(
                self.handle as _,
                JobObjectExtendedLimitInformation,
                (&mut memory as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&memory) as u32,
                std::ptr::null_mut(),
            );
            QueryInformationJobObject(
                self.handle as _,
                JobObjectBasicAccountingInformation,
                (&mut cpu as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                std::mem::size_of_val(&cpu) as u32,
                std::ptr::null_mut(),
            );
            (
                memory.PeakJobMemoryUsed as u64,
                ((cpu.TotalKernelTime + cpu.TotalUserTime).max(0) as u64) / 10_000,
            )
        }
        #[cfg(unix)]
        {
            let memory = fs::read_to_string(format!("/proc/{}/status", self.pid))
                .ok()
                .and_then(|s| {
                    s.lines()
                        .find(|l| l.starts_with("VmHWM:"))
                        .and_then(|l| l.split_whitespace().nth(1))
                        .and_then(|n| n.parse::<u64>().ok())
                })
                .unwrap_or(0)
                * 1024;
            let cpu = fs::read_to_string(format!("/proc/{}/stat", self.pid))
                .ok()
                .and_then(|s| {
                    let fields: Vec<_> = s.rsplit_once(')')?.1.split_whitespace().collect();
                    Some(
                        fields.get(11)?.parse::<u64>().ok()?
                            + fields.get(12)?.parse::<u64>().ok()?,
                    )
                })
                .unwrap_or(0);
            let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
            (memory, cpu * 1000 / hz)
        }
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        self.stop();
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.handle as _);
        }
    }
}

pub struct OwnedChild {
    child: Child,
    pub tree: Arc<Tree>,
}
impl OwnedChild {
    fn spawn(argv: &[String], dir: &Path, manifest: &Manifest, port: Option<u16>) -> Result<Self> {
        let local = dir.join(&argv[0]);
        let executable = if local.is_file() {
            local
        } else if cfg!(windows) && local.with_extension("exe").is_file() {
            local.with_extension("exe")
        } else {
            std::path::PathBuf::from(&argv[0])
        };
        let mut cmd = Command::new(executable);
        // Do not inherit evaluator API credentials into candidate programs.
        cmd.env_clear();
        for name in [
            "PATH",
            "SystemRoot",
            "WINDIR",
            "COMSPEC",
            "PATHEXT",
            "TEMP",
            "TMP",
            "TMPDIR",
            "HOME",
            "USERPROFILE",
            "APPDATA",
            "LOCALAPPDATA",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "ProgramW6432",
            "ProgramData",
            "ALLUSERSPROFILE",
            "SystemDrive",
            "INCLUDE",
            "LIB",
            "LIBPATH",
            "VCINSTALLDIR",
            "VSINSTALLDIR",
            "VCToolsInstallDir",
            "WindowsSdkDir",
            "WindowsSDKVersion",
            "VSCMD_ARG_TGT_ARCH",
            "VSCMD_VER",
            "CARGO_HOME",
            "RUSTUP_HOME",
            "RUSTUP_TOOLCHAIN",
            "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "XDG_RUNTIME_DIR",
        ] {
            if let Some(value) = std::env::var_os(name) {
                cmd.env(name, value);
            }
        }
        cmd.args(&argv[1..])
            .current_dir(dir)
            .envs(&manifest.env)
            .env("HARNESS_PROTOCOL", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(port) = port {
            cmd.env("HARNESS_PORT", port.to_string());
        }
        Self::launch(cmd)
    }
    pub fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        Ok(self.child.try_wait()?)
    }
    pub fn launch(mut cmd: Command) -> Result<Self> {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000004); // No console window; suspend before assigning the job.
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let child = cmd.spawn()?;
        #[cfg(windows)]
        let mut child = child;
        #[cfg(windows)]
        let tree = unsafe {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::JobObjects::*;
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                let _ = child.kill();
                let _ = child.wait();
                return Err(std::io::Error::last_os_error().into());
            }
            let tree = Arc::new(Tree {
                handle: handle as usize,
            });
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&info) as u32,
            ) == 0
                || AssignProcessToJobObject(handle, child.as_raw_handle()) == 0
            {
                let error = std::io::Error::last_os_error();
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.into());
            }
            #[link(name = "ntdll")]
            unsafe extern "system" {
                fn NtResumeProcess(process: *mut std::ffi::c_void) -> i32;
            }
            if NtResumeProcess(child.as_raw_handle()) < 0 {
                tree.stop();
                let _ = child.wait();
                return Err("failed to resume candidate process".into());
            }
            tree
        };
        #[cfg(unix)]
        let tree = Arc::new(Tree { pid: child.id() });
        Ok(Self { child, tree })
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.tree.stop();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn capture(mut reader: impl Read + Send + 'static) -> Arc<Mutex<Vec<u8>>> {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let copy = bytes.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut out = copy.lock().unwrap();
                    let keep = n.min(MAX_LOG.saturating_sub(out.len()));
                    out.extend_from_slice(&buf[..keep]);
                }
            }
        }
    });
    bytes
}

pub fn build(manifest: &Manifest, dir: &Path, timeout: Duration) -> Result<()> {
    let Some(argv) = &manifest.build else {
        return Ok(());
    };
    let mut process = OwnedChild::spawn(argv, dir, manifest, None)?;
    drop(process.child.stdin.take());
    let stdout = capture(process.child.stdout.take().unwrap());
    let stderr = capture(process.child.stderr.take().unwrap());
    let started = Instant::now();
    let result = loop {
        if let Some(status) = process.child.try_wait()? {
            break if status.success() {
                Ok(())
            } else {
                Err(format!("build exited with {status}").into())
            };
        }
        if started.elapsed() >= timeout {
            break Err("build timeout".into());
        }
        thread::sleep(Duration::from_millis(10));
    };
    process.tree.stop();
    let mut log = stdout.lock().unwrap().clone();
    log.extend_from_slice(&stderr.lock().unwrap());
    fs::write(dir.join("harness-build.log"), &log)?;
    result.map_err(|e: crate::Error| {
        format!(
            "{e}: {}",
            String::from_utf8_lossy(&log)
                .chars()
                .take(4000)
                .collect::<String>()
        )
        .into()
    })
}

type Line = std::result::Result<(Instant, Value), String>;
pub struct Protocol {
    pub process: OwnedChild,
    input: SyncSender<Vec<u8>>,
    output: Receiver<Line>,
    stderr: Arc<Mutex<Vec<u8>>>,
    next_id: u64,
}
impl Protocol {
    pub fn launch(manifest: &Manifest, dir: &Path, port: Option<u16>) -> Result<Self> {
        let mut process = OwnedChild::spawn(&manifest.run, dir, manifest, port)?;
        let mut stdin = process.child.stdin.take().unwrap();
        let stdout = process.child.stdout.take().unwrap();
        let stderr = capture(process.child.stderr.take().unwrap());
        let (input, requests) = mpsc::sync_channel::<Vec<u8>>(1);
        let (responses, output) = mpsc::sync_channel(16);
        thread::spawn(move || {
            while let Ok(bytes) = requests.recv() {
                if stdin.write_all(&bytes).and_then(|_| stdin.flush()).is_err() {
                    break;
                }
            }
        });
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut bytes = Vec::new();
                let line = match reader
                    .by_ref()
                    .take((MAX_LINE + 1) as u64)
                    .read_until(b'\n', &mut bytes)
                {
                    Ok(0) => Err("process exited or closed stdout".into()),
                    Ok(n) if n > MAX_LINE || !bytes.ends_with(b"\n") => {
                        Err("unterminated or oversized protocol line".into())
                    }
                    Ok(_) => serde_json::from_slice(&bytes)
                        .map(|v| (Instant::now(), v))
                        .map_err(|e| format!("invalid JSON: {e}")),
                    Err(e) => Err(e.to_string()),
                };
                let failed = line.is_err();
                if responses.try_send(line).is_err() || failed {
                    break;
                }
            }
        });
        let player = Self {
            process,
            input,
            output,
            stderr,
            next_id: 0,
        };
        let ready = player.receive(Instant::now() + START_TIMEOUT)?;
        if ready.get("ready").and_then(Value::as_bool) != Some(true) {
            return Err("first stdout line must be {\"ready\":true}".into());
        }
        Ok(player)
    }
    fn receive(&self, deadline: Instant) -> Result<Value> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match self.output.recv_timeout(remaining) {
            Ok(Ok((at, value))) if at <= deadline => Ok(value),
            Ok(Err(e)) => Err(format!(
                "{e}; stderr: {}",
                String::from_utf8_lossy(&self.stderr.lock().unwrap())
                    .chars()
                    .take(1000)
                    .collect::<String>()
            )
            .into()),
            _ => Err("response timeout".into()),
        }
    }
    pub fn exchange(&mut self, mut value: Value, budget: Duration) -> Result<(Value, f64)> {
        let start = Instant::now();
        self.next_id += 1;
        value["id"] = json!(self.next_id);
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        if bytes.len() > MAX_LINE {
            return Err("request exceeds 64 KiB".into());
        }
        let result = (|| -> Result<Value> {
            self.input
                .try_send(bytes)
                .map_err(|e| format!("input queue failed: {e}"))?;
            let response = self.receive(start + budget)?;
            if response.get("id").and_then(Value::as_u64) != Some(self.next_id) {
                return Err("response id mismatch".into());
            }
            if let Some(error) = response.get("error") {
                return Err(format!("candidate error: {error}").into());
            }
            Ok(response)
        })();
        if result.is_err() {
            self.process.tree.stop();
        }
        Ok((result?, start.elapsed().as_secs_f64() * 1000.0))
    }
}

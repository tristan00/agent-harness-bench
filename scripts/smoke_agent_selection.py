import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

from agent_selection import CATALOG, VENDOR, detect_family, expand_command, planner_candidates, workspace_settings


def main():
    family = detect_family()
    candidates = planner_candidates(family)
    scripts = Path(__file__).resolve().parent
    with tempfile.TemporaryDirectory(prefix='agent-selection-') as directory:
        root = Path(directory)
        capture, prompt = root / 'argv.json', root / 'prompt.txt'
        prompt.write_text('Unchanged prompt')
        (root / 'fake.py').write_text("import json,os,sys,pathlib\npathlib.Path(os.environ['CAPTURE']).write_text(json.dumps(sys.argv[1:]))\nif '--output-last-message' in sys.argv: pathlib.Path(sys.argv[sys.argv.index('--output-last-message')+1]).write_text('{}')\npathlib.Path(os.environ['CAPTURE']+'.prompt').write_text(sys.stdin.read())\n")
        fake = root / 'fake.cmd'
        fake.write_text(f'@"{sys.executable}" "{root / "fake.py"}" %*\n')
        env = {**os.environ, 'CODEX_BIN':str(fake), 'CLAUDE_BIN':str(fake), 'CAPTURE':str(capture), 'HARNESS_PLANNER_PROMPT':str(prompt),
               'HARNESS_CODEX_ARGS':'["--model","gpt-6-astra","-c","model_reasoning_effort=\\"xhigh\\""]'}
        for selection, (model, effort) in candidates.items():
            env['HARNESS_WORKSPACE_PLANNER'] = selection
            subprocess.run([sys.executable, str(scripts / 'template_planner.py')], cwd=root, env=env, check=True, capture_output=True, timeout=10)
            argv = json.loads(capture.read_text())
            assert argv[argv.index('--model') + 1] == model, argv
            if CATALOG[selection]['harness'] == 'codex':
                assert f'model_reasoning_effort="{effort}"' in argv, argv
            else:
                assert argv[argv.index('--effort') + 1] == effort, argv
            assert not any('xhigh' in value or 'astra' in value for value in argv), argv
            assert Path(str(capture) + '.prompt').read_text() == 'Unchanged prompt'
            assert expand_command(['agent', '--model', '{model}', '--effort', '{effort}'], selection) == ['agent', '--model', model, '--effort', effort]
            print(f'{selection}: the planner adapter explicitly uses {model} / {effort}')
        other = [name for name, agent in CATALOG.items() if VENDOR[agent['provider']] != family]
        retired = [name for name, agent in CATALOG.items() if agent.get('retired')]
        allowed = next(iter(candidates))
        for selection in [None, 'astra_xhigh', *other, *retired]:
            try:
                workspace_settings({'planner': selection, 'worker': allowed})
                raise AssertionError(f'invalid candidate accepted: {selection}')
            except ValueError:
                pass
        try:
            expand_command(['agent'], allowed)
            raise AssertionError('implicit agent command accepted')
        except ValueError:
            pass
        print(f'Missing, retired and other-family selections rejected before launch ({family} family)')


if __name__ == '__main__':
    main()

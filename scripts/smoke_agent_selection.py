import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

from agent_selection import PLANNER_CANDIDATES, expand_command, workspace_settings
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'tests'))
from prompt_fixture import selected_documents


def main():
    scripts = Path(__file__).resolve().parent
    with tempfile.TemporaryDirectory(prefix='agent-selection-') as directory:
        root = Path(directory)
        capture, result, request, prompt = [root / name for name in ['argv.json','result.json','request.json','prompt.txt']]
        prompt.write_text('Unchanged prompt')
        (root / 'exec').write_text("import json,os,sys,pathlib\npathlib.Path(os.environ['CAPTURE']).write_text(json.dumps(sys.argv[1:]))\nif '--output-last-message' in sys.argv: pathlib.Path(sys.argv[sys.argv.index('--output-last-message')+1]).write_text('{}')\npathlib.Path(os.environ['CAPTURE']+'.prompt').write_text(sys.stdin.read())\n")
        env = {**os.environ, 'AGENT_FAMILY':'codex', 'CODEX_BIN':sys.executable, 'CAPTURE':str(capture),
               'HARNESS_CODEX_ARGS':'["--model","gpt-6-astra","-c","model_reasoning_effort=\\\"xhigh\\\""]',
               'HARNESS_PLANNER_PROMPT':str(prompt), 'HARNESS_PLANNER_REQUEST':str(request),
               'HARNESS_PLANNER_RESULT':str(result), 'COORDINATOR_REQUEST':str(request), 'COORDINATOR_RESULT':str(result)}
        for selection,(model,effort) in PLANNER_CANDIDATES.items():
            settings=selected_documents(root, {'planner':selection,'worker':selection})
            env['HARNESS_WORKSPACE_PLANNER']=selection
            commands=[([sys.executable,str(scripts/'template_planner.py')],{},'template planner'),
                      ([sys.executable,str(scripts/'codex_planner.py')],{'workspace':str(root),'workspace_settings':settings},'legacy planner')]
            for command,payload,label in commands:
                request.write_text(json.dumps(payload))
                subprocess.run(command,cwd=root,env=env,check=True,capture_output=True,timeout=10)
                argv=json.loads(capture.read_text())
                assert argv[argv.index('--model')+1]==model,(label,argv)
                assert f'model_reasoning_effort="{effort}"' in argv,(label,argv)
                assert not any('xhigh' in value or 'astra' in value for value in argv),(label,argv)
                actual_prompt=Path(str(capture)+'.prompt').read_text()
                if label=='legacy planner':assert 'Selected project_planner fixture' in actual_prompt
            assert expand_command(['agent','--model','{model}','--effort','{effort}'],selection)==['agent','--model',model,'--effort',effort]
            print(f'{selection}: both planner adapters explicitly use {model} / {effort}')
        for selection in [None,'sol_light','luna_6_high','local_qwen3_4b','claude_haiku_5_5_low','astra_xhigh']:
            try:
                workspace_settings({'planner':selection,'worker':'sol_6_1_light'})
                raise AssertionError('invalid candidate accepted')
            except ValueError:pass
        try:
            expand_command(['agent'],'sol_6_1_light')
            raise AssertionError('implicit agent command accepted')
        except ValueError:pass
        print('Missing, retired and unsupported selections rejected before launch')


if __name__=='__main__':
    main()

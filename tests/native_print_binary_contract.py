#!/usr/bin/env python3
"""Frozen AW binary native-print contract; fake only git/container CLI boundaries.

Run: python3 tests/native_print_binary_contract.py --awman /absolute/built/awman
Fixture directories are private and disposable under --fixture-root. No provider runs.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

PARSER = argparse.ArgumentParser()
PARSER.add_argument("--awman", required=True, type=Path)
PARSER.add_argument("--fixture-root", required=True, type=Path)
ARGS, UNIT_ARGS = PARSER.parse_known_args()
PROMPT = "review 'quoted' input\n--model fake\nUnicode: λ\n"
MODEL = "native-model"
FINAL = "/workspace/awman-native-last-message.txt"
STDOUT = b'{"fragment":"one"}\n' + (b"large-final-" * 131072) + "λ-final\n\n".encode()
STDERR = b"PROVIDER_STDERR_BEGIN\n" + (b"diagnostic-final-" * 65536) + b"\nPROVIDER_STDERR_END\n"

DOCKER = r'''#!/usr/bin/python3
import json,os,sys
from pathlib import Path
args=sys.argv[1:]
if args[:2] == ['image','inspect']:
    if '--format' in args: sys.stdout.write('HOME=/root\n')
    sys.exit(0)
if args and args[0] in ['stop','rm']: sys.exit(0)
if not args or args[0] != 'run':
    sys.stderr.write('fixture: unsupported container CLI operation\n')
    sys.exit(93)
Path(os.environ['NATIVE_FIXTURE_ARGV']).write_text(json.dumps(args))
Path(os.environ['NATIVE_FIXTURE_STDIN']).write_bytes(sys.stdin.buffer.read())
stdout=Path(os.environ['NATIVE_FIXTURE_STDOUT']).read_bytes()
stderr=Path(os.environ['NATIVE_FIXTURE_STDERR']).read_bytes()
# Distinct fragments and a large final write exercise both readers and CLI drains.
for data in [stdout[:1],stdout[1:18],stdout[18:-3],stdout[-3:]]:
    while data:
        sent=os.write(1,data)
        data=data[sent:]
for data in [stderr[:9],stderr[9:-2],stderr[-2:]]:
    while data:
        sent=os.write(2,data)
        data=data[sent:]
sys.exit(int(os.environ['NATIVE_FIXTURE_EXIT']))
'''
GIT = r'''#!/usr/bin/python3
import os,sys
if sys.argv[1:] == ['rev-parse','--show-toplevel']:
    print(os.getcwd())
    sys.exit(0)
sys.stderr.write('fixture: unsupported git operation\n')
sys.exit(94)
'''


def expected(agent, session):
    if agent == "codex":
        value = [agent,"exec","--model",MODEL,"--json","--output-last-message",FINAL,"--skip-git-repo-check"]
        if session is not None: value += ["resume",session]
        return value + ["-"]
    value = [agent,"-p",PROMPT,"--model",MODEL,"--output-format","json"]
    if session is not None: value += ["--resume" if agent == "claude" else "--conversation",session]
    return value


class NativePrintBinaryContract(unittest.TestCase):
    def execute(self, agent, code, session=None, empty=False):
        ARGS.fixture_root.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="native-print-", dir=ARGS.fixture_root) as directory:
            root=Path(directory)
            project=root/"project"
            home=root/"child-home"
            config=root/"child-config"
            bins=root/"fake-cli"
            for path in [project/".awman",home,config/"awman",bins,root/"temp"]: path.mkdir(parents=True,exist_ok=True)
            (config/"awman"/"config.json").write_text(json.dumps({"runtime":"docker"}))
            (project/".awman"/"config.json").write_text(json.dumps({"auth":"none"}))
            # AGY uses the existing upstream asset filename; selector remains agy.
            for asset in ["claude","codex","antigravity"]:
                (project/".awman"/("Dockerfile."+asset)).write_text("FROM fixture\nENV HOME=/root\n")
            for relative, content in [
                (".claude/settings.json","{}"),
                (".codex/config.toml",'model = "fixture"\n'),
                (".gemini/antigravity-cli/settings.json","{}"),
            ]:
                target=home/relative
                target.parent.mkdir(parents=True,exist_ok=True)
                target.write_text(content)
            for name,text in [("docker",DOCKER),("git",GIT)]:
                target=bins/name
                target.write_text(text)
                target.chmod(0o700)
            stdout=root/"provider.stdout"
            stderr=root/"provider.stderr"
            stdout.write_bytes(b"" if empty else STDOUT)
            stderr.write_bytes(b"" if empty else STDERR)
            capture=root/"argv.json"
            stdin=root/"stdin.bin"
            # Only the child receives a private HOME. Parent/session HOME is unchanged.
            env={
                "PATH":str(bins)+":/usr/bin:/bin",
                "HOME":str(home),
                "XDG_CONFIG_HOME":str(config),
                "AWMAN_CONFIG_HOME":str(config/"awman"),
                "AWMAN_API_ROOT":str(root/"api"),
                "TMPDIR":str(root/"temp"),
                "NATIVE_FIXTURE_ARGV":str(capture),
                "NATIVE_FIXTURE_STDIN":str(stdin),
                "NATIVE_FIXTURE_STDOUT":str(stdout),
                "NATIVE_FIXTURE_STDERR":str(stderr),
                "NATIVE_FIXTURE_EXIT":str(code),
            }
            command=[str(ARGS.awman.resolve()),"exec","prompt","--native-print","--agent",agent,"--model",MODEL]
            if session is not None: command += ["--native-session",session]
            command += [PROMPT]
            # communicate waits for actual process exit and pipe EOF; no completion sleep.
            output=subprocess.run(command,cwd=project,env=env,input=b"",stdout=subprocess.PIPE,stderr=subprocess.PIPE,timeout=20)
            self.assertTrue(capture.is_file(), f"AW never reached fake container boundary: exit={output.returncode}, stderr={output.stderr[-400:]!r}")
            argv=json.loads(capture.read_text())
            image=next((i for i,a in enumerate(argv) if a.startswith("awman-") and a.endswith(":latest")),None)
            self.assertIsNotNone(image,"actual runtime image argument absent")
            # Image names may also occur in --name; the image immediately precedes provider.
            provider=next((i for i,a in enumerate(argv) if a == agent),None)
            self.assertIsNotNone(provider,"canonical provider entrypoint absent")
            self.assertEqual(argv[provider:],expected(agent,session))
            self.assertNotIn("-t",argv)
            self.assertNotIn("-it",argv)
            self.assertEqual(stdin.read_bytes(),PROMPT.encode() if agent == "codex" else b"")
            self.assertEqual(output.returncode,code,"provider nonzero exit must reach AW process status")
            self.assertEqual(output.stdout,b"" if empty else STDOUT,"provider stdout must be byte-exact through AW process exit")
            if not empty:
                start=output.stderr.find(b"PROVIDER_STDERR_BEGIN\n")
                self.assertGreaterEqual(start,0,"provider stderr beginning lost")
                self.assertEqual(output.stderr[start:start+len(STDERR)],STDERR,"provider stderr final chunks lost or interleaved")
                self.assertNotIn(b"PROVIDER_STDERR",output.stderr[start+len(STDERR):])
            self.assertEqual((home/".codex/config.toml").read_text(),'model = "fixture"\n')
            self.assertEqual((home/".gemini/antigravity-cli/settings.json").read_text(),"{}")
            return output

    def test_first_and_resumed_turns_fully_drain_both_streams(self):
        for agent in ["claude","codex","agy"]:
            for session in [None,"provider-session-1"]:
                with self.subTest(agent=agent,session=session): self.execute(agent,0,session)

    def test_nonzero_provider_exit_has_complete_native_stdout_and_no_aw_summary(self):
        for agent in ["claude","codex","agy"]:
            with self.subTest(agent=agent): self.execute(agent,7)

    def test_zero_output_provider_exit_remains_nonzero_with_empty_stdout(self):
        self.execute("codex",7,empty=True)


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]]+UNIT_ARGS)

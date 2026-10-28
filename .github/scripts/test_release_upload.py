import contextlib
import importlib.util
import io
import json
import os
import re
import textwrap
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
SOURCE = Path(__file__).with_name('release_upload.py')
spec = importlib.util.spec_from_file_location('release_upload', SOURCE)
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)

CI_JOBS = {
    'Stable / ubuntu-latest', 'Stable / macos-latest',
    'Rust 1.85.1 / ubuntu-latest', 'Rust 1.85.1 / macos-latest',
    'Documentation', 'Dart 3.13.0 minimum / ubuntu-latest',
    'Flutter 3.47.7 / ubuntu-latest', 'Flutter 3.47.7 / macos-latest',
    'Flutter stable / ubuntu-latest', 'Flutter stable / macos-latest',
}
WINDOWS_JOBS = {
    'Native Rust / windows-latest', 'Native Dart compiler launch paths',
    'Flutter 3.47.7 / windows-latest', 'Flutter stable / windows-latest',
}
WORKFLOWS = {
    'ci': ('ci.yml', 'CI', 11, 22, CI_JOBS),
    'windows': ('windows.yml', 'Windows native', 33, 44, WINDOWS_JOBS),
}


class GitHubFixture(module.GitHub):
    def __init__(self, scenario='success', *, workflow=None, defect=None):
        self.scenario = scenario
        self.workflow = workflow
        self.defect = defect
        self.calls = []

    def request(self, path, *, asset=None):
        self.calls.append((path, asset))
        if path == '/user':
            return {'login': 'unexpected-account' if self.scenario == 'account' else module.ACCOUNT}
        if asset:
            return {'id': 123, 'name': asset.name, 'state': 'uploaded', 'size': asset.size,
                    'uploader': {'login': module.ACCOUNT}, 'digest': f'sha256:{asset.sha256}'}
        legacy = {'workflow': 'workflow-name', 'ci-sha': 'sha',
            'ci-failure': 'failure', 'ci-skipped': 'job-skipped'}
        for scope, (filename, name, workflow_id, run_id, required) in WORKFLOWS.items():
            defect = self.defect if scope == self.workflow else None
            if scope == 'ci' and self.scenario in legacy:
                defect = legacy[self.scenario]
            workflow_path = f'.github/workflows/{filename}'
            if path.endswith(f'/actions/workflows/{filename}'):
                value = {'id': workflow_id, 'name': name, 'path': workflow_path}
                if defect == 'workflow-name':
                    value['name'] = 'other'
                if defect == 'workflow-path':
                    value['path'] = '.github/workflows/other.yml'
                if defect == 'workflow-id':
                    value['id'] = 0
                if defect == 'workflow-bool-id':
                    value['id'] = True
                return value
            if f'/actions/workflows/{workflow_id}/runs?' in path:
                run = {'id': run_id, 'name': name, 'path': workflow_path,
                    'workflow_id': workflow_id, 'head_sha': 'a' * 40,
                    'status': 'completed', 'conclusion': 'success', 'event': 'push',
                    'head_repository': {'full_name': module.REPOSITORY},
                    'repository': {'full_name': module.REPOSITORY}}
                for field, bad in [('name', 'other'), ('path', 'other.yml'),
                        ('workflow_id', 999), ('id', None), ('head_sha', 'b' * 40),
                        ('status', 'in_progress'), ('event', 'pull_request')]:
                    if defect == {'head_sha': 'sha', 'workflow_id': 'run-workflow-id',
                            'id': 'run-id', 'status': 'pending'}.get(field, f'run-{field}'):
                        run[field] = bad
                if defect == 'failure':
                    run['conclusion'] = 'failure'
                if defect == 'head-repository':
                    run['head_repository']['full_name'] = 'other/fail-forge'
                if defect == 'repository':
                    run['repository']['full_name'] = 'other/fail-forge'
                return {'workflow_runs': [] if defect == 'missing' else [run]}
            if f'/actions/runs/{run_id}/jobs?' in path:
                jobs = [{'name': name, 'run_id': run_id, 'head_sha': 'a' * 40,
                    'status': 'completed', 'conclusion': 'success'} for name in sorted(required)]
                if defect == 'job-skipped':
                    jobs[0]['conclusion'] = 'skipped'
                if defect == 'job-failure':
                    jobs[0]['conclusion'] = 'failure'
                if defect == 'job-pending':
                    jobs[0]['status'] = 'queued'
                if defect == 'job-name':
                    jobs[0]['name'] = 'unexpected job'
                if defect == 'job-duplicate':
                    jobs[-1]['name'] = jobs[0]['name']
                if defect == 'job-run-id':
                    jobs[0]['run_id'] = 999
                if defect == 'job-sha':
                    jobs[0]['head_sha'] = 'b' * 40
                if defect == 'job-missing':
                    jobs.pop()
                return {'total_count': len(jobs) + (defect == 'job-count'), 'jobs': jobs}
        if '/git/ref/tags/' in path:
            return {'ref': 'refs/tags/v0.1.0-beta.3', 'object': {
                'type': 'tag' if self.scenario == 'annotated' else 'commit',
                'sha': 'b' * 40 if self.scenario in ('tag', 'annotated') else 'a' * 40}}
        if '/git/tags/' in path:
            return {'object': {'type': 'commit', 'sha': 'a' * 40}}
        if '/releases?' in path or path.endswith('/releases/99'):
            release = {'id': 99, 'draft': self.scenario != 'published', 'prerelease': True,
                'tag_name': 'v0.1.0-beta.3',
                'target_commitish': 'main' if self.scenario == 'commitish' else 'a' * 40,
                'upload_url': f'https://uploads.github.com/repos/{module.REPOSITORY}/releases/99/assets{{?name,label}}'}
            if '/releases?' in path:
                return [release, release] if self.scenario == 'duplicate' else [release]
            return release
        if '/releases/99/assets?' in path:
            return [{'name': 'package-manifest-x86_64-unknown-linux-gnu.json'}] if self.scenario == 'collision' else []
        raise AssertionError(path)


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='failforge-release-validation-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.options = SimpleNamespace(version='0.1.0-beta.3', source_sha='a' * 40,
            target='x86_64-unknown-linux-gnu', package_dir=self.root,
            smoke_summary=self.root / 'smoke.json')
        self.archive_name = 'failforge-0.1.0-beta.3-x86_64-unknown-linux-gnu.tar.gz'
        (self.root / self.archive_name).write_bytes(b'fixture archive')
        self.manifest = {'schemaVersion': 1, 'version': self.options.version,
            'target': self.options.target, 'archiveRoot': self.archive_name[:-7],
            'archive': self.archive_name, 'source': {'revision': 'a' * 40, 'dirty': False}}
        self.write_json('package-manifest.json', self.manifest)
        self.manifest_hash = module.digest(self.root / 'package-manifest.json')
        (self.root / 'SHA256SUMS').write_text(
            f'{module.digest(self.root / self.archive_name)}  {self.archive_name}\n'
            f'{self.manifest_hash}  package-manifest.json\n')
        self.smoke = {'allPassed': True, 'status': 'completed', 'offline': False,
            'cacheMode': 'new-isolated-cache', 'temporaryInputsRemoved': True,
            'ownerCleanupConfirmed': True, 'error': None,
            'results': {'packageVersion': self.options.version,
                'packageManifestSha256': self.manifest_hash, 'payloadUnchanged': True,
                'directYamlParity': True, 'sourceUnchanged': True, 'cleanupConfirmed': True}}
        self.write_json('smoke.json', self.smoke)

    def write_json(self, name, value):
        (self.root / name).write_text(json.dumps(value))

    def use_target(self, target):
        self.options.target = target
        self.archive_name = f'failforge-{self.options.version}-{target}.tar.gz'
        (self.root / self.archive_name).write_bytes(b'fixture archive')
        self.manifest.update(target=target, archive=self.archive_name, archiveRoot=self.archive_name[:-7])
        self.write_json('package-manifest.json', self.manifest)
        self.manifest_hash = module.digest(self.root / 'package-manifest.json')
        (self.root / 'SHA256SUMS').write_text(
            f'{module.digest(self.root / self.archive_name)}  {self.archive_name}\n'
            f'{self.manifest_hash}  package-manifest.json\n')
        self.smoke['results']['packageManifestSha256'] = self.manifest_hash
        self.write_json('smoke.json', self.smoke)

    def test_validation_only_needs_no_credentials_or_network(self):
        for target in ['x86_64-unknown-linux-gnu', 'aarch64-apple-darwin', 'x86_64-pc-windows-msvc']:
            self.use_target(target)
            with self.subTest(target=target):
                result = subprocess.run([sys.executable, str(SOURCE), '--version', self.options.version,
                    '--source-sha', self.options.source_sha, '--target', self.options.target,
                    '--package-dir', str(self.root), '--smoke-summary', str(self.options.smoke_summary),
                    '--validate-only'], capture_output=True, text=True, check=True)
                self.assertEqual(len(json.loads(result.stdout)['assets']), 3)

    def test_actual_workflow_input_and_actor_guards(self):
        workflow = SOURCE.parent.parent / 'workflows' / 'release.yml'
        match = re.search(r'^        shell: python\n        run: \|\n((?:          [^\n]*\n|\s*\n)+)',
            workflow.read_text(), re.MULTILINE)
        self.assertIsNotNone(match)
        validation = textwrap.dedent(match[1])
        environment = {'RELEASE_VERSION': self.options.version, 'RELEASE_SOURCE_SHA': self.options.source_sha,
            'GITHUB_REPOSITORY': module.REPOSITORY, 'GITHUB_ACTOR': module.ACCOUNT,
            'GITHUB_TRIGGERING_ACTOR': module.ACCOUNT, 'GITHUB_REF': 'refs/heads/main'}
        for ref in ['refs/heads/main', 'refs/tags/v0.1.0-beta.3']:
            with patch.dict(os.environ, dict(environment, GITHUB_REF=ref), clear=True):
                exec(compile(validation, '<workflow-validation>', 'exec'), {})
        for field, value in [('RELEASE_VERSION', '0.01.0-beta.3'), ('RELEASE_SOURCE_SHA', 'A' * 40),
                ('GITHUB_REPOSITORY', 'other/fail-forge'), ('GITHUB_ACTOR', 'unexpected-account'),
                ('GITHUB_REF', 'refs/heads/untrusted'), ('GITHUB_TRIGGERING_ACTOR', 'unexpected-account')]:
            with self.subTest(field=field), patch.dict(os.environ, dict(environment, **{field: value}), clear=True):
                with self.assertRaises(SystemExit):
                    exec(compile(validation, '<workflow-validation>', 'exec'), {})

    def test_companion_names_and_checksums_match_uploaded_names(self):
        assets = module.local_assets(self.options)
        self.assertEqual(assets[1].name, 'package-manifest-x86_64-unknown-linux-gnu.json')
        self.assertEqual(assets[2].name, 'SHA256SUMS-x86_64-unknown-linux-gnu')
        self.assertEqual(assets[2].body.decode().splitlines(), [
            f'{assets[0].sha256}  {assets[0].name}', f'{assets[1].sha256}  {assets[1].name}'])

    def test_windows_target_has_distinct_archive_manifest_and_checksum_names(self):
        self.use_target('x86_64-pc-windows-msvc')
        assets = module.local_assets(self.options)
        self.assertEqual([asset.name for asset in assets], [self.archive_name,
            'package-manifest-x86_64-pc-windows-msvc.json', 'SHA256SUMS-x86_64-pc-windows-msvc'])
        self.assertEqual(assets[2].body.decode().splitlines(), [
            f'{assets[0].sha256}  {assets[0].name}', f'{assets[1].sha256}  {assets[1].name}'])

    def test_dirty_or_wrong_source_is_rejected(self):
        for source in ({'revision': 'a' * 40, 'dirty': True}, {'revision': 'b' * 40, 'dirty': False}):
            self.manifest['source'] = source
            self.write_json('package-manifest.json', self.manifest)
            with self.assertRaises(module.ReleaseError):
                module.local_assets(self.options)

    def test_archive_mutation_is_rejected(self):
        (self.root / self.archive_name).write_bytes(b'changed')
        with self.assertRaises(module.ReleaseError):
            module.local_assets(self.options)

    def test_local_package_inputs_cannot_be_symlinks(self):
        for name in ['package-manifest.json', self.archive_name, 'SHA256SUMS', 'smoke.json']:
            path = self.root / name
            original = self.root / (name + '.original')
            path.rename(original)
            path.symlink_to(original.name)
            with self.subTest(name=name), self.assertRaises(module.ReleaseError):
                module.local_assets(self.options)
            path.unlink()
            original.rename(path)

    def test_actual_transport_sends_verified_file_body_and_headers(self):
        asset = module.local_assets(self.options)[0]
        captured = {}

        def open_request(request, timeout):
            captured.update(method=request.get_method(), url=request.full_url,
                headers=dict(request.header_items()), body=request.data.read(), timeout=timeout)
            response = io.BytesIO(b'{"accepted": true}')
            response.status = 201
            return response

        github = module.GitHub('test-token')
        github.opener = SimpleNamespace(open=open_request)
        endpoint = f'https://uploads.github.com/repos/{module.REPOSITORY}/releases/99/assets?name={asset.name}'
        self.assertEqual(github.request(endpoint, asset=asset), {'accepted': True})
        self.assertEqual(captured['method'], 'POST')
        self.assertEqual(captured['headers']['Content-length'], str(asset.size))
        self.assertEqual(captured['headers']['Content-type'], 'application/gzip')
        self.assertEqual(captured['body'], (self.root / self.archive_name).read_bytes())
        self.assertEqual(captured['headers']['Authorization'], 'Bearer test-token')

    def test_windows_transport_does_not_fall_back_to_missing_posix_flags(self):
        asset = module.local_assets(self.options)[0]
        native_open = lambda path: os.open(path, os.O_RDONLY | (os.O_BINARY if os.name == 'nt' else 0))
        github = module.GitHub('test-token')
        response = io.BytesIO(b'{}')
        response.status = 201
        github.opener = SimpleNamespace(open=lambda request, timeout: response)
        with patch.object(module.sys, 'platform', 'win32'), \
                patch.object(module.os, 'O_NOFOLLOW', None, create=True), \
                patch.object(module.os, 'O_NONBLOCK', None, create=True), \
                patch.object(module, '_open_windows_descriptor', side_effect=native_open, create=True) as opened:
            github.request(f'https://uploads.github.com/repos/{module.REPOSITORY}/releases/99/assets', asset=asset)
        opened.assert_called_once_with(asset.body)

    def test_transport_rejects_same_bytes_replaced_file_before_streaming(self):
        asset = module.local_assets(self.options)[0]
        replacement = self.root / 'replacement'
        replacement.write_bytes(asset.body.read_bytes())
        original_open = getattr(module, '_open_descriptor', None)
        sent = []

        def swap_then_open(path):
            os.replace(replacement, path)
            return original_open(path)

        github = module.GitHub('test-token')

        def send(*args, **kwargs):
            sent.append(True)
            response = io.BytesIO(b'{}')
            response.status = 201
            return response

        github.opener = SimpleNamespace(open=send)
        with patch.object(module, '_open_descriptor', side_effect=swap_then_open, create=True):
            with self.assertRaises(module.ReleaseError):
                github.request(f'https://uploads.github.com/repos/{module.REPOSITORY}/releases/99/assets', asset=asset)
        self.assertFalse(sent)

    def test_transport_rejects_alias_swapped_after_metadata_check(self):
        asset = module.local_assets(self.options)[0]
        original_open = module._open_descriptor
        original = self.root / 'original-archive'
        sent = []

        def replace_with_alias(path):
            path.rename(original)
            path.symlink_to(original.name)
            return original_open(path)

        github = module.GitHub('test-token')
        github.opener = SimpleNamespace(open=lambda *args, **kwargs: sent.append(True))
        with patch.object(module, '_open_descriptor', side_effect=replace_with_alias):
            with self.assertRaises(module.ReleaseError):
                github.request(f'https://uploads.github.com/repos/{module.REPOSITORY}/releases/99/assets', asset=asset)
        self.assertFalse(sent)
        asset.body.unlink()  # A rejected native reparse handle must already be closed.

    @unittest.skipUnless(sys.platform == 'win32', 'native Windows file-sharing semantics')
    def test_windows_transport_retains_handle_against_write_and_rename_during_send(self):
        asset = module.local_assets(self.options)[0]
        expected = asset.body.read_bytes()
        github = module.GitHub('test-token')

        def send(request, timeout):
            with self.assertRaises(OSError):
                with asset.body.open('r+b'):
                    pass
            with self.assertRaises(OSError):
                asset.body.rename(self.root / 'renamed')
            self.assertEqual(request.data.read(), expected)
            response = io.BytesIO(b'{}')
            response.status = 201
            return response

        github.opener = SimpleNamespace(open=send)
        github.request(f'https://uploads.github.com/repos/{module.REPOSITORY}/releases/99/assets', asset=asset)

    def test_http_failure_does_not_expose_response_body_or_token(self):
        github = module.GitHub('private-test-token')

        def fail_request(request, timeout):
            raise module.urllib.error.HTTPError(request.full_url, 422, 'private-test-token',
                {}, io.BytesIO(b'private-test-token'))

        github.opener = SimpleNamespace(open=fail_request)
        with self.assertRaises(module.ReleaseError) as captured:
            github.request('/user')
        self.assertIn('HTTP 422', str(captured.exception))
        self.assertNotIn('private-test-token', str(captured.exception))

    def test_smoke_must_be_fresh_and_bound_to_manifest(self):
        for field, value in [('offline', True), ('allPassed', False), ('cacheMode', 'existing-caller-cache')]:
            smoke = dict(self.smoke, **{field: value})
            self.write_json('smoke.json', smoke)
            with self.assertRaises(module.ReleaseError):
                module.local_assets(self.options)
        smoke = dict(self.smoke, results=dict(self.smoke['results'], packageManifestSha256='b' * 64))
        self.write_json('smoke.json', smoke)
        with self.assertRaises(module.ReleaseError):
            module.local_assets(self.options)

    def test_wrong_account_performs_no_other_requests(self):
        github = GitHubFixture('account')
        with self.assertRaises(module.ReleaseError):
            github.upload(self.options, module.local_assets(self.options))
        self.assertEqual([call[0] for call in github.calls], ['/user'])

    def test_remote_guards_fail_before_any_upload(self):
        assets = module.local_assets(self.options)
        for scenario in ['workflow', 'ci-sha', 'ci-failure', 'tag', 'published', 'commitish', 'collision', 'duplicate', 'ci-skipped']:
            github = GitHubFixture(scenario)
            with self.subTest(scenario=scenario), contextlib.redirect_stdout(io.StringIO()):
                with self.assertRaises(module.ReleaseError):
                    github.upload(self.options, assets)
            self.assertFalse(any(asset for _, asset in github.calls))

    def test_every_target_requires_both_exact_source_workflows_before_upload(self):
        defects = ['workflow-name', 'workflow-path', 'workflow-id', 'workflow-bool-id',
            'missing', 'sha', 'failure', 'pending', 'head-repository', 'repository',
            'run-name', 'run-path', 'run-workflow-id', 'run-id', 'run-event',
            'job-skipped', 'job-failure', 'job-pending', 'job-name', 'job-duplicate',
            'job-run-id', 'job-sha', 'job-missing', 'job-count']
        for target in ['x86_64-unknown-linux-gnu', 'aarch64-apple-darwin', 'x86_64-pc-windows-msvc']:
            self.use_target(target)
            assets = module.local_assets(self.options)
            for workflow in WORKFLOWS:
                for defect in defects:
                    github = GitHubFixture(workflow=workflow, defect=defect)
                    with self.subTest(target=target, workflow=workflow, defect=defect), \
                            contextlib.redirect_stdout(io.StringIO()):
                        with self.assertRaises(module.ReleaseError):
                            github.upload(self.options, assets)
                        self.assertFalse(any(asset for _, asset in github.calls))

    def test_successful_both_workflows_are_checked_before_first_upload(self):
        github = GitHubFixture()
        with contextlib.redirect_stdout(io.StringIO()):
            github.upload(self.options, module.local_assets(self.options))
        first_upload = next(index for index, (_, asset) in enumerate(github.calls) if asset)
        verified = [path for path, _ in github.calls[:first_upload]]
        self.assertTrue(any('/actions/runs/22/jobs?' in path for path in verified))
        self.assertTrue(any('/actions/runs/44/jobs?' in path for path in verified))

    def test_success_and_annotated_tag_upload_three_assets_without_publish(self):
        for target in ['x86_64-unknown-linux-gnu', 'aarch64-apple-darwin', 'x86_64-pc-windows-msvc']:
            self.use_target(target)
            assets = module.local_assets(self.options)
            for scenario in ['success', 'annotated']:
                github = GitHubFixture(scenario)
                with self.subTest(target=target, scenario=scenario), contextlib.redirect_stdout(io.StringIO()):
                    github.upload(self.options, assets)
                self.assertEqual(sum(asset is not None for _, asset in github.calls), 3)
                self.assertEqual(github.calls[0][0], '/user')
                self.assertTrue(all('/assets?name=' in path for path, asset in github.calls if asset))
                self.assertFalse(any('/releases/tags/' in path for path, _ in github.calls))

    def test_package_change_after_validation_stops_before_upload(self):
        assets = module.local_assets(self.options)
        (self.root / self.archive_name).write_bytes(b'changed after validation')
        github = GitHubFixture()
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(module.ReleaseError):
            github.upload(self.options, assets)
        self.assertFalse(any(asset for _, asset in github.calls))


if __name__ == '__main__':
    unittest.main()

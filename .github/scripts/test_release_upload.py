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


class GitHubFixture(module.GitHub):
    def __init__(self, scenario='success'):
        self.scenario = scenario
        self.calls = []

    def request(self, path, *, asset=None):
        self.calls.append((path, asset))
        if path == '/user':
            return {'login': 'unexpected-account' if self.scenario == 'account' else module.ACCOUNT}
        if asset:
            return {'id': 123, 'name': asset.name, 'state': 'uploaded', 'size': asset.size,
                    'uploader': {'login': module.ACCOUNT}, 'digest': f'sha256:{asset.sha256}'}
        if path.endswith('/actions/workflows/ci.yml'):
            return {'id': 11, 'name': 'other' if self.scenario == 'workflow' else 'CI', 'path': '.github/workflows/ci.yml'}
        if '/actions/workflows/11/runs?' in path:
            return {'workflow_runs': [{'id': 22, 'name': 'CI', 'workflow_id': 11,
                'head_sha': 'b' * 40 if self.scenario == 'ci-sha' else 'a' * 40,
                'status': 'completed', 'conclusion': 'failure' if self.scenario == 'ci-failure' else 'success',
                'event': 'push', 'head_repository': {'full_name': module.REPOSITORY}}]}
        if '/actions/runs/22/jobs?' in path:
            jobs = [{'name': name, 'status': 'completed', 'conclusion': 'success'} for name in sorted(module.CI_JOBS)]
            if self.scenario == 'ci-skipped':
                jobs[0]['conclusion'] = 'skipped'
            return {'total_count': len(jobs), 'jobs': jobs}
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

    def test_validation_only_needs_no_credentials_or_network(self):
        result = subprocess.run([sys.executable, str(SOURCE), '--version', self.options.version,
            '--source-sha', self.options.source_sha, '--target', self.options.target,
            '--package-dir', str(self.root), '--smoke-summary', str(self.options.smoke_summary),
            '--validate-only'], capture_output=True, text=True, check=True)
        self.assertEqual(len(json.loads(result.stdout)['assets']), 3)

    def test_actual_workflow_input_and_actor_guards(self):
        workflow = SOURCE.parent.parent / 'workflows' / 'release.yml'
        match = re.search(r"python3 - <<'PY'\n(.*?)\n\s*PY\n", workflow.read_text(), re.DOTALL)
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

    def test_success_and_annotated_tag_upload_three_assets_without_publish(self):
        assets = module.local_assets(self.options)
        for scenario in ['success', 'annotated']:
            github = GitHubFixture(scenario)
            with contextlib.redirect_stdout(io.StringIO()):
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

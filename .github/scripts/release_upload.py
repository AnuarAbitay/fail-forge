#!/usr/bin/env python3
"""Upload verified native packages to an existing draft with a personal token."""

import argparse
from contextlib import nullcontext
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import sys
import urllib.error
import urllib.parse
import urllib.request


ACCOUNT = 'AnuarAbitay'
REPOSITORY = 'AnuarAbitay/fail-forge'
API_BASE = 'https://api.github.com'
TARGETS = {'x86_64-unknown-linux-gnu', 'aarch64-apple-darwin'}
NUMBER = r'(?:0|[1-9][0-9]*)'
VERSION = re.compile(rf'{NUMBER}\.{NUMBER}\.{NUMBER}-beta\.{NUMBER}')
SHA = re.compile(r'[0-9a-f]{40}')
CI_JOBS = {
    'Stable / ubuntu-latest', 'Stable / macos-latest',
    'Rust 1.85.1 / ubuntu-latest', 'Rust 1.85.1 / macos-latest',
    'Documentation', 'Dart 3.13.0 minimum / ubuntu-latest',
    'Flutter 3.47.7 / ubuntu-latest', 'Flutter 3.47.7 / macos-latest',
    'Flutter stable / ubuntu-latest', 'Flutter stable / macos-latest',
}


class ReleaseError(Exception):
    pass


def require(condition, message):
    if not condition:
        raise ReleaseError(message)


def digest(path):
    result = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            result.update(block)
    return result.hexdigest()


def read_json(path):
    try:
        with path.open(encoding='utf-8') as stream:
            value = json.load(stream)
    except (OSError, ValueError):
        raise ReleaseError(f'Cannot read JSON file: {path.name}') from None
    require(isinstance(value, dict), f'Expected an object in {path.name}')
    return value


@dataclass(frozen=True)
class Asset:
    name: str
    body: object
    sha256: str
    size: int
    content_type: str


def local_assets(options):
    require(VERSION.fullmatch(options.version), 'Expected an exact SemVer beta version.')
    require(SHA.fullmatch(options.source_sha), 'Expected a full immutable commit SHA.')
    require(options.target in TARGETS, 'Unsupported native release target.')
    package = options.package_dir.resolve(strict=True)
    manifest_path = package / 'package-manifest.json'
    require(manifest_path.is_file() and not manifest_path.is_symlink(), 'Missing regular package manifest.')
    manifest = read_json(manifest_path)
    archive_root = f'failforge-{options.version}-{options.target}'
    archive_name = f'{archive_root}.tar.gz'
    require(
        manifest.get('schemaVersion') == 1
        and manifest.get('version') == options.version
        and manifest.get('target') == options.target
        and manifest.get('archiveRoot') == archive_root
        and manifest.get('archive') == archive_name,
        'Package identity does not match the requested release.',
    )
    source = manifest.get('source', {})
    require(
        isinstance(source, dict)
        and source.get('revision') == options.source_sha
        and source.get('dirty') is False,
        'Package must come from the exact clean tagged revision.',
    )
    archive = package / archive_name
    require(archive.is_file() and not archive.is_symlink(), 'Missing regular native archive.')
    manifest_hash = digest(manifest_path)
    archive_hash = digest(archive)
    expected = {archive_name: archive_hash, 'package-manifest.json': manifest_hash}
    sums_path = package / 'SHA256SUMS'
    require(sums_path.is_file() and not sums_path.is_symlink(), 'Missing regular package checksums.')
    try:
        lines = sums_path.read_text(encoding='ascii').splitlines()
    except (OSError, ValueError):
        raise ReleaseError('Cannot read original package checksums.') from None
    actual = {}
    for line in lines:
        match = re.fullmatch(r'([0-9a-f]{64})  ([A-Za-z0-9._-]+)', line)
        require(match is not None and match[2] not in actual, 'Invalid or duplicate original checksum.')
        actual[match[2]] = match[1]
    require(actual == expected, 'Archive or manifest does not match original package checksums.')
    require(options.smoke_summary.is_file() and not options.smoke_summary.is_symlink(), 'Missing regular smoke summary.')
    smoke = read_json(options.smoke_summary)
    results = smoke.get('results') or {}
    require(
        isinstance(results, dict)
        and smoke.get('allPassed') is True
        and smoke.get('status') == 'completed'
        and smoke.get('offline') is False
        and smoke.get('cacheMode') == 'new-isolated-cache'
        and smoke.get('temporaryInputsRemoved') is True
        and smoke.get('ownerCleanupConfirmed') is True
        and smoke.get('error') is None
        and results.get('packageVersion') == options.version
        and results.get('packageManifestSha256') == manifest_hash
        and all(results.get(key) is True for key in (
            'payloadUnchanged', 'directYamlParity', 'sourceUnchanged', 'cleanupConfirmed',
        )),
        'Fresh-cache package smoke is incomplete or belongs to another package.',
    )
    manifest_name = f'package-manifest-{options.target}.json'
    sums_name = f'SHA256SUMS-{options.target}'
    sums = f'{archive_hash}  {archive_name}\n{manifest_hash}  {manifest_name}\n'.encode('ascii')
    return [
        Asset(archive_name, archive, archive_hash, archive.stat().st_size, 'application/gzip'),
        Asset(manifest_name, manifest_path, manifest_hash, manifest_path.stat().st_size, 'application/json'),
        Asset(sums_name, sums, hashlib.sha256(sums).hexdigest(), len(sums), 'text/plain'),
    ]


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, response, code, message, headers, new_url):
        return None


class GitHub:
    def __init__(self, token):
        require(bool(token), 'RELEASE_UPLOAD_TOKEN is required; no credential fallback is permitted.')
        self.token = token
        self.opener = urllib.request.build_opener(NoRedirect())

    def request(self, path, *, asset=None):
        if asset is None:
            url = API_BASE + path
        else:
            url = path
            require(url.startswith(f'https://uploads.github.com/repos/{REPOSITORY}/releases/'), 'Unexpected upload endpoint.')
        headers = {
            'Authorization': 'Bearer ' + self.token,
            'Accept': 'application/vnd.github+json',
            'X-GitHub-Api-Version': '2022-11-28',
            'User-Agent': 'FailForge-release',
        }
        body_context = nullcontext(None)
        if asset is not None:
            headers.update({'Content-Type': asset.content_type, 'Content-Length': str(asset.size)})
            if isinstance(asset.body, Path):
                descriptor = os.open(asset.body, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
                body_context = os.fdopen(descriptor, 'rb')
            else:
                body_context = nullcontext(asset.body)
        try:
            with body_context as body:
                if asset is not None and isinstance(asset.body, Path):
                    metadata = os.fstat(body.fileno())
                    require(stat.S_ISREG(metadata.st_mode) and metadata.st_size == asset.size, 'Upload input is not the expected regular file.')
                    hashed = hashlib.sha256()
                    for block in iter(lambda: body.read(1024 * 1024), b''):
                        hashed.update(block)
                    require(hashed.hexdigest() == asset.sha256, 'Opened upload input differs from the verified package.')
                    body.seek(0)
                request = urllib.request.Request(url, data=body, headers=headers, method='POST' if asset else 'GET')
                with self.opener.open(request, timeout=120 if asset else 45) as response:
                    require(response.status == (201 if asset else 200), 'Unexpected GitHub response status.')
                    value = json.load(response)
        except urllib.error.HTTPError as error:
            raise ReleaseError(f'GitHub request failed with HTTP {error.code}; nothing will be overwritten.') from None
        except (urllib.error.URLError, OSError, ValueError):
            raise ReleaseError('GitHub request failed; inspect the draft before retrying.') from None
        return value

    def verify_account(self):
        identity = self.request('/user')
        require(isinstance(identity, dict) and identity.get('login') == ACCOUNT, 'GitHub identity does not match AnuarAbitay; stopped.')
        print('Verified GitHub account: AnuarAbitay', flush=True)

    def verify_ci(self, source_sha):
        workflow = self.request(f'/repos/{REPOSITORY}/actions/workflows/ci.yml')
        require(
            isinstance(workflow, dict)
            and workflow.get('name') == 'CI'
            and workflow.get('path') == '.github/workflows/ci.yml'
            and isinstance(workflow.get('id'), int),
            'Expected the repository CI workflow.',
        )
        query = urllib.parse.urlencode({'head_sha': source_sha, 'status': 'success', 'per_page': 100})
        response = self.request(f"/repos/{REPOSITORY}/actions/workflows/{workflow['id']}/runs?{query}")
        require(isinstance(response, dict) and isinstance(response.get('workflow_runs'), list), 'Unexpected CI run inventory.')
        runs = [run for run in response['workflow_runs'] if (
            run.get('name') == 'CI'
            and run.get('workflow_id') == workflow['id']
            and run.get('head_sha') == source_sha
            and run.get('status') == 'completed'
            and run.get('conclusion') == 'success'
            and run.get('event') in ('push', 'workflow_dispatch')
            and run.get('head_repository', {}).get('full_name') == REPOSITORY
        )]
        for run in runs:
            response = self.request(f"/repos/{REPOSITORY}/actions/runs/{run['id']}/jobs?filter=latest&per_page=100")
            require(isinstance(response, dict) and isinstance(response.get('jobs'), list), 'Unexpected CI job inventory.')
            jobs = response['jobs']
            if (response.get('total_count') == len(CI_JOBS)
                    and len(jobs) == len(CI_JOBS)
                    and {job.get('name') for job in jobs} == CI_JOBS
                    and all(job.get('status') == 'completed' and job.get('conclusion') == 'success' for job in jobs)):
                print(json.dumps({'verifiedCiRun': run['id'], 'sourceSha': source_sha, 'successfulJobs': len(jobs)}), flush=True)
                return
        raise ReleaseError('The exact source SHA must have a successful CI run with all 10 required jobs.')

    def verify_tag(self, tag, source_sha):
        ref = self.request(f'/repos/{REPOSITORY}/git/ref/tags/{tag}')
        require(ref.get('ref') == f'refs/tags/{tag}', 'Unexpected release tag reference.')
        target = ref.get('object', {})
        seen = set()
        for _ in range(16):
            revision = target.get('sha', '')
            require(SHA.fullmatch(revision) and revision not in seen, 'Invalid or cyclic release tag.')
            seen.add(revision)
            if target.get('type') == 'commit':
                require(revision == source_sha, 'Release tag does not reference the exact source SHA.')
                return
            require(target.get('type') == 'tag', 'Release tag does not resolve to a commit.')
            target = self.request(f'/repos/{REPOSITORY}/git/tags/{revision}').get('object', {})
        raise ReleaseError('Release tag nesting exceeds the supported limit.')

    def validate_draft(self, release, tag, source_sha):
        require(
            isinstance(release, dict)
            and release.get('draft') is True
            and release.get('prerelease') is True
            and release.get('tag_name') == tag
            and release.get('target_commitish') == source_sha
            and isinstance(release.get('id'), int)
            and release['id'] > 0,
            'Expected the pre-created draft prerelease targeting the exact source SHA.',
        )
        endpoint = f"https://uploads.github.com/repos/{REPOSITORY}/releases/{release['id']}/assets"
        require(release.get('upload_url') == endpoint + '{?name,label}', 'Unexpected draft upload URL.')
        return release, endpoint

    def find_draft(self, tag, source_sha):
        matches = []
        for page in range(1, 101):
            rows = self.request(f'/repos/{REPOSITORY}/releases?per_page=100&page={page}')
            require(isinstance(rows, list) and all(isinstance(row, dict) for row in rows), 'Unexpected release inventory.')
            matches.extend(row for row in rows if row.get('tag_name') == tag)
            if len(rows) < 100:
                break
        else:
            raise ReleaseError('Release inventory exceeds the supported limit.')
        require(len(matches) == 1, 'Expected exactly one pre-created release for the version tag.')
        return self.validate_draft(matches[0], tag, source_sha)

    def draft(self, tag, source_sha, release_id):
        release, endpoint = self.validate_draft(
            self.request(f'/repos/{REPOSITORY}/releases/{release_id}'), tag, source_sha,
        )
        require(release['id'] == release_id, 'Draft release ID changed.')
        return release, endpoint

    def upload(self, options, assets):
        self.verify_account()
        self.verify_ci(options.source_sha)
        tag = f'v{options.version}'
        self.verify_tag(tag, options.source_sha)
        release, endpoint = self.find_draft(tag, options.source_sha)
        existing = set()
        for page in range(1, 101):
            rows = self.request(f"/repos/{REPOSITORY}/releases/{release['id']}/assets?per_page=100&page={page}")
            require(isinstance(rows, list), 'Unexpected release asset inventory.')
            existing.update(row.get('name') for row in rows)
            if len(rows) < 100:
                break
        else:
            raise ReleaseError('Release asset inventory exceeds the supported limit.')
        require(not existing.intersection(asset.name for asset in assets), 'A planned asset name already exists; refusing to overwrite it.')
        for asset in assets:
            current, current_endpoint = self.draft(tag, options.source_sha, release['id'])
            require(current['id'] == release['id'] and current_endpoint == endpoint, 'Draft release identity changed during upload.')
            if isinstance(asset.body, Path):
                require(digest(asset.body) == asset.sha256, 'Package input changed before upload.')
            uploaded = self.request(endpoint + '?' + urllib.parse.urlencode({'name': asset.name}), asset=asset)
            require(
                uploaded.get('name') == asset.name
                and uploaded.get('state') == 'uploaded'
                and uploaded.get('size') == asset.size
                and uploaded.get('uploader', {}).get('login') == ACCOUNT
                and uploaded.get('digest') in (None, f'sha256:{asset.sha256}'),
                'Uploaded asset metadata does not match the verified package and account.',
            )
            print(json.dumps({'uploaded': asset.name, 'assetId': uploaded.get('id'), 'sha256': asset.sha256}), flush=True)
        self.draft(tag, options.source_sha, release['id'])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--version', required=True)
    parser.add_argument('--source-sha', required=True)
    parser.add_argument('--target', required=True, choices=sorted(TARGETS))
    parser.add_argument('--package-dir', required=True, type=Path)
    parser.add_argument('--smoke-summary', required=True, type=Path)
    parser.add_argument('--validate-only', action='store_true', help='Verify local release inputs without credentials or network access.')
    options = parser.parse_args()
    try:
        assets = local_assets(options)
        if options.validate_only:
            print(json.dumps({'version': options.version, 'sourceSha': options.source_sha, 'target': options.target, 'assets': [asset.name for asset in assets]}))
            return 0
        GitHub(os.environ.get('RELEASE_UPLOAD_TOKEN', '')).upload(options, assets)
        return 0
    except (ReleaseError, OSError) as error:
        print(f'release upload: {error}', file=sys.stderr)
        return 1


if __name__ == '__main__':
    sys.exit(main())

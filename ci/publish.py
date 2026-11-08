#!/usr/bin/env python3
"""Publish verified desktop downloads with this project's CI_JOB_TOKEN."""
import hashlib
import json
import os
from pathlib import Path
import re
from urllib.error import HTTPError
from urllib.parse import quote
from urllib.request import Request, urlopen


def api(method, path, data=None, *, missing_ok=False):
    body = data if isinstance(data, bytes) else json.dumps(data).encode() if data is not None else None
    headers = {'JOB-TOKEN': os.environ['CI_JOB_TOKEN']}
    if body is not None:
        headers['Content-Type'] = 'application/octet-stream' if isinstance(data, bytes) else 'application/json'
    request = Request(f'{os.environ["CI_API_V4_URL"]}/projects/{os.environ["CI_PROJECT_ID"]}/{path}',
                      data=body, headers=headers, method=method)
    try:
        with urlopen(request, timeout=120) as response:
            return json.load(response)
    except HTTPError as error:
        error.close()
        if missing_ok and error.code == 404:
            return None
        raise RuntimeError(f'{method} {path} failed: HTTP {error.code}') from None


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def publish():
    tag = os.environ['RELEASE_TAG']
    if not re.fullmatch(r'v\d+\.\d+\.\d+', tag):
        raise ValueError('Expected a numeric vMAJOR.MINOR.PATCH tag')
    root = Path('release')
    files = [root / name for name in ('Pneu-macOS.zip', f'pneu_{tag[1:]}_amd64.deb', 'pneu-linux-x86_64.tar.gz')]
    hashes = [digest(path) for path in files]
    expected = (root / 'macOS-SHA256SUMS.txt').read_text().strip()
    if expected != f'{hashes[0]}  Pneu-macOS.zip':
        raise ValueError('macOS artifact checksum mismatch')
    sums = root / 'SHA256SUMS.txt'
    sums.write_text(''.join(f'{value}  {path.name}\n' for path, value in zip(files, hashes)))
    files.append(sums)
    # A retry gets a new job ID: uploaded packages are immutable, release links move.
    version = quote(f'{tag}-{os.environ["CI_JOB_ID"]}', safe='')
    links = []
    for path in files:
        package = f'packages/generic/pneu/{version}/{quote(path.name, safe="")}'
        api('PUT', package, path.read_bytes())
        links.append({'name': path.name,
                      'url': f'{os.environ["CI_API_V4_URL"]}/projects/{os.environ["CI_PROJECT_ID"]}/{package}',
                      'direct_asset_path': f'/{path.name}', 'link_type': 'package'})
    release_path = f'releases/{quote(tag, safe="")}'
    existing = api('GET', release_path, missing_ok=True)
    details = {'name': f'Pneu {tag}',
               'description': 'Downloads and installation: https://stantonmatthewj.gitlab.io/pneu/'}
    if existing is None:
        api('POST', 'releases', dict(details, tag_name=tag, assets={'links': links}))
    else:
        api('PUT', release_path, details)
        old_links = {link['name']: link['id'] for link in existing['assets']['links']}
        for link in links:
            old_id = old_links.get(link['name'])
            api('PUT' if old_id is not None else 'POST',
                f'{release_path}/assets/links' + (f'/{old_id}' if old_id is not None else ''), link)
    print(f'Published {tag} with {len(files)} verified assets')


if __name__ == '__main__':
    publish()

#!/usr/bin/env python3
"""Private SQLite maintenance; run as the backend UID, once per hour.

Only expired ephemeral rows are deleted. Device/owner/public-key tombstones are
permanent. Online Backup API includes committed WAL without copying live DB files.
"""
from contextlib import closing
import argparse
import os
from pathlib import Path
import sqlite3
import stat
import time


def private(path, directory=False):
    path = Path(path)
    info = path.lstat()
    kind = stat.S_ISDIR if directory else stat.S_ISREG
    if not kind(info.st_mode) or info.st_uid != os.geteuid() or stat.S_IMODE(info.st_mode) & 0o077:
        raise ValueError('maintenance requires private, owned, nonsymlink paths')
    return path


def connect(path):
    path = private(path)
    private(path.parent, directory=True)
    connection = sqlite3.connect(path.resolve().as_uri() + '?mode=rw', uri=True, timeout=10)
    connection.execute('PRAGMA foreign_keys=ON')
    return connection


def prune(path):
    # One-hour grace plus hourly schedule bounds retention to two hours past expiry.
    cutoff = int(time.time() * 1000) - 3_600_000
    with closing(connect(path)) as db, db:
        db.execute('BEGIN IMMEDIATE')
        for table in ('oauth_transactions', 'confirmations', 'device_challenges', 'access_tokens'):
            db.execute(f'DELETE FROM {table} WHERE expires_at <= ?', (cutoff,))
        db.execute('''DELETE FROM enrollments WHERE expires_at <= ?
            AND NOT EXISTS(SELECT 1 FROM oauth_transactions o WHERE o.enrollment_id=enrollments.id)
            AND NOT EXISTS(SELECT 1 FROM confirmations c WHERE c.enrollment_id=enrollments.id)''', (cutoff,))
        # Already closed sessions cannot authorize a write; leave active sessions untouched.
        db.execute('DELETE FROM sessions WHERE closed=1 AND lease_expires_at <= ?', (cutoff,))


def backup(path, destination):
    destination = Path(destination)
    private(destination.parent, directory=True)
    with closing(connect(path)) as source:
        fd = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        os.close(fd)
        try:
            with closing(sqlite3.connect(destination)) as target, target:
                source.backup(target, pages=128, sleep=0.05)
                if target.execute('PRAGMA integrity_check').fetchone() != ('ok',):
                    raise ValueError('backup integrity check failed')
            with destination.open('rb') as saved:
                os.fsync(saved.fileno())
        except BaseException:
            destination.unlink()
            raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('operation', choices=('prune', 'backup'))
    parser.add_argument('database', type=Path)
    parser.add_argument('destination', type=Path, nargs='?')
    args = parser.parse_args()
    if args.operation == 'prune':
        if args.destination:
            parser.error('prune has no destination')
        prune(args.database)
    else:
        if not args.destination:
            parser.error('backup requires a fresh destination in a private directory')
        backup(args.database, args.destination)
    print('Private SQLite maintenance completed')


if __name__ == '__main__':
    main()

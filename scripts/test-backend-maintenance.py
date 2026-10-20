#!/usr/bin/env python3
import importlib.util
from pathlib import Path
import os
import sqlite3
import tempfile
import unittest

REPO = Path(__file__).resolve().parent.parent


class Maintenance(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.directory = Path(self.tmp.name)
        self.directory.chmod(0o700)
        self.path = self.directory / 'backend.db'
        self.db = sqlite3.connect(self.path)
        self.path.chmod(0o600)
        for migration in sorted((REPO / 'apps/backend/migrations').glob('*.sql')):
            self.db.executescript(migration.read_text())
        self.db.execute("INSERT INTO users VALUES('owner')")
        self.db.execute("INSERT INTO devices VALUES('revoked','owner','device','key',1)")
        self.db.execute("INSERT INTO devices VALUES('active','owner','device','key2',0)")
        for suffix, expiry in [('old', 1), ('live', 9999999999999)]:
            self.db.execute('INSERT INTO enrollments VALUES(?,?,?,?,?,?,?,?,?,?)', (suffix,suffix,'device',suffix,'code','verifier','challenge',expiry,None,0))
            self.db.execute('INSERT INTO oauth_transactions VALUES(?,?,?,?,?)', (suffix,suffix,'nonce','pkce',expiry))
            self.db.execute('INSERT INTO confirmations VALUES(?,?,?,?)', (suffix,suffix,'owner',expiry))
            self.db.execute('INSERT INTO device_challenges VALUES(?,?,?,?,?)', (suffix,'active','challenge',expiry,0))
            self.db.execute('INSERT INTO access_tokens VALUES(?,?,?)', (suffix,'active',expiry))
        self.db.commit()
        spec = importlib.util.spec_from_file_location('maintenance', REPO / 'packaging/backend/maintenance.py')
        self.module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.module)

    def tearDown(self):
        self.db.close()
        self.tmp.cleanup()

    def test_prune_retains_live_records_and_revoked_ownership(self):
        self.module.prune(self.path)
        for table in ('enrollments','oauth_transactions','confirmations','device_challenges','access_tokens'):
            self.assertEqual(self.db.execute(f'SELECT COUNT(*) FROM {table}').fetchone()[0], 1)
        self.assertEqual(self.db.execute("SELECT owner,public_key,revoked FROM devices WHERE id='revoked'").fetchone(), ('owner','key',1))
        self.assertEqual(self.db.execute('PRAGMA foreign_key_check').fetchall(), [])

    def test_consistent_private_backup_includes_committed_wal(self):
        self.db.execute('PRAGMA journal_mode=WAL')
        self.db.execute("INSERT INTO users VALUES('wal-committed')")
        self.db.commit()
        target = self.directory / 'backup.db'
        self.module.backup(self.path, target)
        self.assertEqual(target.stat().st_mode & 0o777, 0o600)
        with sqlite3.connect(target) as saved:
            self.assertEqual(saved.execute('PRAGMA integrity_check').fetchone()[0], 'ok')
            self.assertTrue(saved.execute("SELECT subject FROM users WHERE subject='wal-committed'").fetchone())
        with self.assertRaises(FileExistsError):
            self.module.backup(self.path, target)

    def test_rejects_public_or_symlink_database_and_public_backup_parent(self):
        self.path.chmod(0o644)
        with self.assertRaises(ValueError):
            self.module.prune(self.path)
        self.path.chmod(0o600)
        link = self.directory / 'link.db'
        link.symlink_to(self.path)
        with self.assertRaises(ValueError):
            self.module.prune(link)
        public = self.directory / 'public'
        public.mkdir(mode=0o755)
        with self.assertRaises(ValueError):
            self.module.backup(self.path, public / 'backup.db')


unittest.main()

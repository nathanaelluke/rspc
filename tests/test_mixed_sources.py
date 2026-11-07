#!/usr/bin/env python3
"""Shared queue tests: real MPD decoder/null output, fake Soloist, rspc frontend.

Uses private sockets, WAVs and XDG directories. Never reads a real Spotify key,
contacts Spotify, plays system audio, or modifies the user's MPD/library.
"""
import json
import functools
import http.server
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import time
import threading
import unittest
import wave

import test_daemon as daemon_tests
BIN, ROOT = daemon_tests.BIN, daemon_tests.ROOT


@unittest.skipUnless(shutil.which('mpd'), 'MPD executable required')
class MixedSourceTest(unittest.TestCase):
    cli = daemon_tests.DaemonTest.cli
    raw = daemon_tests.DaemonTest.raw
    status = daemon_tests.DaemonTest.status
    tui = daemon_tests.DaemonTest.tui

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='rspc-mixed-test-')
        self.base = Path(self.tmp.name)
        self.env = dict(os.environ, TERM='xterm-256color')
        self.env.pop('TMUX', None)
        self.env.pop('RSPC_MPD_OUTPUT', None)
        self.env.pop('RSPC_MPD_PASSWORD', None)
        for key in ('CONFIG', 'STATE', 'CACHE', 'DATA', 'RUNTIME'):
            path=self.base/key.lower();path.mkdir(mode=0o700)
            self.env[f'XDG_{key}_HOME' if key!='RUNTIME' else 'XDG_RUNTIME_DIR']=str(path)
        self.sock=self.base/'engine.sock'
        self.mpd_sock=self.base/'mpd.sock'
        self.env.update(RSPC_MPD_ADDRESS=str(self.mpd_sock),
                        RSPC_SOLOIST_PATH=str(ROOT/'tests/fake_soloist.py'),
                        RSPC_SOLOIST_API_KEY='isolated-test-fixture')
        self.cmd=[str(BIN), '--socket',str(self.sock), '--frontend',str(ROOT/'target/release/rspc-ui')]
        music=self.base/'music';music.mkdir()
        for name,seconds in [('short.wav',.45),('long.wav',8),('sentinel.wav',1)]:
            with wave.open(str(music/name),'wb') as wav:
                wav.setnchannels(1);wav.setsampwidth(2);wav.setframerate(8000)
                wav.writeframes(b'\0\0'*int(8000*seconds))
        (self.base/'playlists').mkdir()
        conf=self.base/'mpd.conf'
        conf.write_text(f'''music_directory {json.dumps(str(music))}
db_file {json.dumps(str(self.base/'db'))}
playlist_directory {json.dumps(str(self.base/'playlists'))}
bind_to_address {json.dumps(str(self.mpd_sock))}
audio_output {{
 type "null"
 name "Isolated fixture"
 mixer_type "software"
}}
''')
        self.log=(self.base/'mpd.log').open('w')
        self.mpd_proc=subprocess.Popen([shutil.which('mpd'),'--no-daemon','--stdout',str(conf)],
                                      env=self.env,stdout=self.log,stderr=subprocess.STDOUT)
        self.addCleanup(self.cleanup)
        self.wait(lambda:self.mpd_sock.exists())
        self.mpd('update')
        self.wait(lambda:'songs: 3' in self.mpd('stats'))
        self.mpd('addid "sentinel.wav"')
        def track(uri,name,ms):
            return dict(uri=uri,name=name,artists=['Fixture Artist'],album='Mixed Queue',duration_ms=ms)
        entries=[track('spotify:track:first','Morning Light',1200),
                 track('mpd:short.wav','short.wav',450),
                 track('spotify:track:last','Last Spotify Song',1200),
                 track('mpd:long.wav','long.wav',8000)]
        state=Path(self.env['XDG_STATE_HOME'])/'rspc';state.mkdir(mode=0o700)
        (state/'queue.json').write_text(json.dumps(dict(entries=[dict(id=i+1,track=t) for i,t in enumerate(entries)],next_id=4,version=1,current=None)))
        self.cli('queue')
        self.wait(lambda:'logged_in: 1\n' in self.doctor())

    def cleanup(self):
        try:
            subprocess.run(self.cmd+['stop'],env=self.env,capture_output=True,timeout=10)
        finally:
            self.mpd_proc.terminate()
            try:self.mpd_proc.wait(timeout=5)
            except subprocess.TimeoutExpired:self.mpd_proc.kill();self.mpd_proc.wait()
            self.log.close();self.tmp.cleanup()

    def wait(self, predicate, seconds=7):
        until=time.monotonic()+seconds
        while time.monotonic()<until:
            if predicate():return
            if getattr(self,'mpd_proc',None) and self.mpd_proc.poll() is not None:
                self.fail((self.base/'mpd.log').read_text())
            time.sleep(.02)
        self.fail('Timed out waiting for fixture state')

    def doctor(self):
        return subprocess.check_output(self.cmd+['doctor'],env=self.env,text=True,timeout=5)

    def mpd(self,command,partition='default'):
        with socket.socket(socket.AF_UNIX) as s:
            s.settimeout(5);s.connect(str(self.mpd_sock));f=s.makefile('rwb');f.readline()
            def send(text):
                f.write((text+'\n').encode());f.flush();lines=[]
                while True:
                    line=f.readline().decode().strip()
                    if line=='OK':return lines
                    self.assertTrue(line and not line.startswith('ACK '),line)
                    lines.append(line)
            send('partition '+json.dumps(partition))
            return send(command)

    def test_local_selection_info_keeps_default_mpd_playback_and_queue_untouched(self):
        before_queue = self.mpd('playlistinfo')
        before_outputs = self.mpd('outputs')
        before_state = next(line for line in self.mpd('status') if line.startswith('state: '))
        reply = self.raw('rspc_selection_info "path" "Local Files"')
        info = dict(json.loads(reply.removeprefix('selection_info: ').strip()))
        self.assertEqual(info['Songs'], '3')
        self.assertEqual(self.raw('listallinfo "Local Files"').count('file: '), 3)
        self.mpd('playlistadd "Folder Info" "long.wav"')
        reply = self.raw('rspc_selection_info "path" "Local Playlists"')
        self.assertEqual(dict(json.loads(reply.removeprefix('selection_info: ').strip()))['Playlists'], '1')
        reply = self.raw('rspc_selection_info "playlist" "Local Playlists/Folder Info"')
        self.assertEqual(dict(json.loads(reply.removeprefix('selection_info: ').strip()))['Songs'], '1')
        self.assertEqual(self.mpd('playlistinfo'), before_queue)
        self.assertEqual(self.mpd('outputs'), before_outputs)
        self.assertEqual(next(line for line in self.mpd('status') if line.startswith('state: ')), before_state)

    def spotify(self):
        path=Path(self.env['XDG_STATE_HOME'])/'rspc/soloist/fixture-state.json'
        try:return json.loads(path.read_text())
        except (FileNotFoundError,ValueError):return {}

    def test_natural_spotify_local_spotify_handoff_preserves_the_shared_queue(self):
        self.cli('play','0')
        self.wait(lambda:self.status().get('songid')==2 and str(self.status()['state']).lower()=='play')
        self.assertIn(self.spotify().get('status'),('paused','idle'))
        self.assertIn('state: play',self.mpd('status','rspc'))
        self.wait(lambda:self.status().get('songid')==3 and str(self.status()['state']).lower()=='play')
        self.assertIn('state: stop',self.mpd('status','rspc'))
        self.assertIn('outputname: Isolated fixture',self.mpd('outputs'))
        self.assertEqual([e['id'] for e in json.loads(self.cli('queue'))],[1,2,3,4])
        self.assertEqual(self.mpd('playlistinfo').count('file: sentinel.wav'),1)

    def test_slow_handoff_does_not_block_new_frontend_connections(self):
        fixture=Path(self.env['XDG_STATE_HOME'])/'rspc/soloist'
        (fixture/'fixture-delays.json').write_text(json.dumps({'play:spotify:track:last':1.5}))
        self.cli('play','0')
        self.wait(lambda:(fixture/'fixture-pending.json').exists())
        start=time.monotonic()
        self.raw('ping')
        self.assertLess(time.monotonic()-start,.5,
                        'A delayed playback acknowledgement blocked the daemon listener')
        self.assertEqual([e['id'] for e in json.loads(self.cli('queue'))],[1,2,3,4])

    def test_connect_reactivation_restores_song_and_notifies_the_ui_without_controls(self):
        fixture=Path(self.env['XDG_STATE_HOME'])/'rspc/soloist'
        def remote(**state):
            pending=fixture/'fixture-playback.tmp'
            pending.write_text(json.dumps(state))
            pending.replace(fixture/'fixture-playback.json')

        self.cli('play','0')
        self.wait(lambda:str(self.status()['state']).lower()=='play')
        original=[entry['id'] for entry in json.loads(self.cli('queue'))]
        remote(active=False,status='paused',position=200,duration=60000)
        self.wait(lambda:str(self.status()['state']).lower()=='stop')
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(3)
            client.connect(str(self.sock))
            stream=client.makefile('rwb')
            self.assertTrue(stream.readline().startswith(b'OK MPD '))
            stream.write(b'status\n');stream.flush()
            while (line:=stream.readline())!=b'OK\n':
                self.assertTrue(line and not line.startswith(b'ACK '),line)
            stream.write(b'idle player\n');stream.flush()
            remote(active=True,status='playing',position=500)
            while True:
                self.assertEqual(stream.readline(),b'changed: player\n')
                self.assertEqual(stream.readline(),b'OK\n')
                if str(self.status()['state']).lower()=='play':break
                stream.write(b'idle player\n');stream.flush()
        self.assertEqual(self.status()['songid'],1)
        self.assertIn('file: spotify:track:first',self.raw('currentsong'))
        self.assertEqual([entry['id'] for entry in json.loads(self.cli('queue'))],original)

    def test_controls_and_ui_exit_keep_local_playback_and_handoff_working(self):
        self.cli('play','3')
        self.wait(lambda:str(self.status()['state']).lower()=='play')
        self.cli('volume','23');self.cli('seek','2');self.cli('pause')
        before=self.status();time.sleep(.1);after=self.status()
        self.assertEqual(str(after['state']).lower(),'pause')
        self.assertAlmostEqual(before['elapsed'],after['elapsed'],delta=.05)
        self.assertIn('volume: 23',self.mpd('status','rspc'))
        self.cli('play');self.tui()
        before=self.status();time.sleep(.15);after=self.status()
        self.assertGreater(after['elapsed'],before['elapsed'])
        self.assertIn('active_source: mpd',self.doctor())
        self.raw('playid 3')
        self.wait(lambda:'active_source: spotify' in self.doctor() and str(self.status()['state']).lower()=='play')
        self.wait(lambda:self.spotify().get('volume')==23)
        self.assertIn('state: stop',self.mpd('status','rspc'))
        self.assertEqual(self.mpd('playlistinfo').count('file: sentinel.wav'),1)

    def test_local_browsing_search_and_queue_edits_do_not_require_a_spotify_catalog_login(self):
        self.assertIn('directory: Local Files',self.raw('lsinfo'))
        self.assertIn('file: mpd:long.wav',self.raw('lsinfo "Local Files"'))
        self.assertIn('Title: long.wav',self.raw('find "(file == \'mpd:long.wav\')"'))
        self.assertIn('file: mpd:long.wav',self.raw('search "(any contains \'long\')"'))
        self.cli('add',str(self.base/'music/long.wav'))
        self.assertEqual(self.status()['playlistlength'],5, 'rspc frontend extension checks must accept MPD decoders')
        new_id=int(self.raw('addid "mpd:long.wav"').split(': ')[1])
        self.raw(f'moveid {new_id} 0')
        self.raw(f'playid {new_id}')
        self.wait(lambda:str(self.status()['state']).lower()=='play')
        self.assertEqual(self.status()['songid'],new_id)
        self.raw('move 0 3')
        self.assertEqual(self.status()['songid'],new_id)
        self.raw(f'deleteid {new_id}')
        self.assertNotIn(new_id,[e['id'] for e in json.loads(self.cli('queue'))])

    def test_missing_local_file_reports_an_error_without_skipping_the_queue(self):
        self.cli('play','3')
        self.wait(lambda:str(self.status()['state']).lower()=='play')
        before=[e['id'] for e in json.loads(self.cli('queue'))]
        # A persisted entry can become unavailable after the library changes.
        (self.base/'music/long.wav').unlink()
        self.cli('stop')
        result=subprocess.run(self.cmd+['--','raw','playid 4'],env=self.env,capture_output=True,text=True,timeout=10)
        self.wait(lambda:str(self.status()['state']).lower()=='stop' and bool(self.status().get('error')))
        self.assertEqual([e['id'] for e in json.loads(self.cli('queue'))],before)
        self.assertIn('outputname: Isolated fixture',self.mpd('outputs'))

    def test_local_playback_works_without_soloist_or_spotify_credentials(self):
        subprocess.run(self.cmd+['stop'],env=self.env,capture_output=True,check=True,timeout=10)
        self.env['RSPC_SOLOIST_PATH']=str(self.base/'not-installed')
        self.env.pop('RSPC_SOLOIST_API_KEY')
        self.cli('play','3')
        self.wait(lambda:str(self.status()['state']).lower()=='play')
        self.assertIn('active_source: mpd',self.doctor())
        self.cli('pause');self.assertEqual(str(self.status()['state']).lower(),'pause')
        self.cli('play');self.assertEqual(str(self.status()['state']).lower(),'play')
        self.assertIn('file: mpd:long.wav',self.raw('lsinfo "Local Files"'))

    def test_local_playlists_can_be_saved_and_edited_without_spotify_login(self):
        self.cli('clear')
        self.cli('add',str(self.base/'music/long.wav'))
        self.cli('save','Local Playlists/Fixture')
        self.raw('playlistadd "Local Playlists/Fixture" "mpd:short.wav"')
        self.assertIn('file: mpd:short.wav',self.raw('listplaylistinfo "Local Playlists/Fixture"'))
        self.raw('playlistmove "Local Playlists/Fixture" 1 0')
        self.raw('playlistdelete "Local Playlists/Fixture" 1')
        self.assertNotIn('long.wav',self.raw('listplaylistinfo "Local Playlists/Fixture"'))
        self.assertEqual(self.status()['playlistlength'],1)
        self.raw('rename "Local Playlists/Fixture" "Local Playlists/Renamed"')
        self.raw('rm "Local Playlists/Renamed"')
        self.assertNotIn('Renamed',self.raw('listplaylists'))

    def test_audio_url_uses_mpd_in_the_same_queue(self):
        class Handler(http.server.SimpleHTTPRequestHandler):
            def log_message(self,*args):pass
        server=http.server.ThreadingHTTPServer(('127.0.0.1',0),functools.partial(Handler,directory=str(self.base/'music')))
        thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
        self.addCleanup(server.server_close);self.addCleanup(server.shutdown)
        uri=f'http://127.0.0.1:{server.server_port}/long.wav'
        new_id=int(self.raw('addid '+json.dumps(uri)).split(': ')[1])
        self.raw(f'playid {new_id}')
        self.wait(lambda:str(self.status()['state']).lower()=='play')
        self.assertIn('active_source: mpd',self.doctor())
        self.assertIn('file: mpd:'+uri,self.raw('currentsong'))
        self.assertEqual(self.status()['playlistlength'],5)
        self.cli('stop')

    def test_mpd_database_changes_notify_the_unified_browser(self):
        # Ensure the lazy local index is populated before changing the database.
        self.raw('search "(any contains \'long\')"')
        shutil.copyfile(self.base/'music/long.wav',self.base/'music/new.wav')
        with socket.socket(socket.AF_UNIX) as s:
            s.settimeout(5);s.connect(str(self.sock));f=s.makefile('rwb');f.readline()
            f.write(b'idle database\n');f.flush()
            self.mpd('update')
            self.assertEqual(f.readline(),b'changed: database\n')
        self.wait(lambda:'songs: 4' in self.mpd('stats'))
        self.wait(lambda:'file: mpd:new.wav' in self.raw('search "(any contains \'new.wav\')"'))


if __name__=='__main__':
    unittest.main(verbosity=2)

#!/usr/bin/env python3
"""Isolated Spotify control fixture. Produces no audio and makes no network requests."""
import argparse
import base64
import hashlib
import json
import select
import socket
import struct
import time
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument('-D', required=True)
args, _ = parser.parse_known_args()
root = Path(args.D)
root.mkdir(parents=True, exist_ok=True)
server = socket.socket()
server.bind(('127.0.0.1', 0))
server.listen()
(root/'soloist.pid').write_text(str(__import__('os').getpid()))
(root/'ws.port').write_text(str(server.getsockname()[1]))
uri, status, position, at, volume, active = '', 'idle', 0, time.monotonic(), 37, True
duration = 1200


def send(sock, value):
    data = json.dumps(value).encode()
    header = bytes([0x81, len(data)]) if len(data)<126 else bytes([0x81,126])+struct.pack('!H',len(data))
    sock.sendall(header+data)


def receive(sock):
    def exact(n):
        data=b''
        while len(data)<n:
            more=sock.recv(n-len(data))
            if not more: raise EOFError
            data+=more
        return data
    first, second=exact(2)
    n=second&127
    if n==126: n=struct.unpack('!H',exact(2))[0]
    elif n==127: n=struct.unpack('!Q',exact(8))[0]
    mask=exact(4) if second&128 else b'\0'*4
    data=bytes(v^mask[i%4] for i,v in enumerate(exact(n)))
    if first&15==8: raise EOFError
    if first&15!=1: return {}
    return json.loads(data)


def snapshot(sock):
    send(sock,dict(type='playback_state',status=status,is_active=active,volume=volume,
        item=dict(uri=uri,decorations=dict(identity=dict(name='Morning Light' if uri.endswith('first') else 'Last Spotify Song'),playback=dict(duration_ms=duration))),
        position=dict(position_ms=int(position),timestamp_ms=int(time.time()*1000),speed=1 if status=='playing' else 0)))


while True:
    client,_=server.accept()
    try:
        headers=b''
        while not headers.endswith(b'\r\n\r\n'): headers+=client.recv(1)
        key=next(l.split(b':',1)[1].strip() for l in headers.split(b'\r\n') if l.lower().startswith(b'sec-websocket-key:'))
        accept=base64.b64encode(hashlib.sha1(key+b'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest())
        client.sendall(b'HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: '+accept+b'\r\n\r\n')
        send(client,dict(type='auth_state',logged_in=True,is_active=active))
        while True:
            tick=time.monotonic()
            remote_path=root/'fixture-playback.json'
            if remote_path.exists():
                remote=json.loads(remote_path.read_text())
                remote_path.unlink()
                active=remote.get('active',active)
                uri=remote.get('uri',uri)
                status=remote.get('status',status)
                position=remote.get('position',position)
                duration=remote.get('duration',duration)
                at=tick
                send(client,dict(type='device_changed',is_active=active))
                snapshot(client)
            if status=='playing': position+=(tick-at)*1000
            at=tick
            if status=='playing' and position>=duration:
                status='idle';position=duration;snapshot(client)
            pending=root/'fixture-state.tmp'
            pending.write_text(json.dumps(dict(uri=uri,status=status,position=position,active=active,volume=volume)))
            pending.replace(root/'fixture-state.json')
            if not select.select([client],[],[],.01)[0]: continue
            v=receive(client);name=v.get('command')
            if not name: continue
            if name=='get_state': snapshot(client);continue
            if name=='set_shuffle':
                send(client,dict(type='playback_state',status='playing',is_active=True,item=dict(uri='spotify:track:first',decorations=dict(playback=dict(duration_ms=duration)))))
            if name=='set_repeat_context':
                send(client,dict(type='track_changed',item=dict(uri='spotify:track:restored-context')))
            if name=='activate': active=True;send(client,dict(type='device_changed',is_active=True))
            elif name=='set_volume': volume=v['volume'];send(client,dict(type='volume_changed',volume=volume))
            elif name=='pause': status='paused';send(client,dict(type='playback_changed',status=status))
            elif name=='play':
                if v.get('uri'): uri=v['uri'];position=0
                status='playing';snapshot(client)
            elif name=='seek': position=v['position_ms'];snapshot(client)
            delays_path=root/'fixture-delays.json'
            delays=json.loads(delays_path.read_text()) if delays_path.exists() else {}
            delay=delays.get(name+':'+v.get('uri',''),delays.get(name,0))
            if delay:
                (root/'fixture-pending.json').write_text(json.dumps(dict(command=name,uri=v.get('uri'))))
                time.sleep(delay)
            send(client,dict(type='command_result',command=name))
    except (EOFError, OSError, ValueError):
        client.close()

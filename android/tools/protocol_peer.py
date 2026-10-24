#!/usr/bin/env python3
"""Minimal independent TLS peer for Android smoke tests, not a desktop implementation.

Uses Python/OpenSSL's normal TLS client-certificate verification. Supply the phone's
public Keystore leaf as --client-ca. Never logs pairing tokens or user bytes.
"""
import argparse
import base64
import hashlib
import json
import socket
import ssl
import struct
import time
import os
import sys
import threading

parser = argparse.ArgumentParser()
parser.add_argument('--receive-files', action='store_true')
parser.add_argument('--hello-version',type=int,default=1)
parser.add_argument('--incompatible-capabilities',action='store_true')
parser.add_argument('--version-mismatch',action='store_true')
parser.add_argument('--receive-progress-delay',type=float,default=0)
parser.add_argument('--mesh', action='store_true')
parser.add_argument('--cert', required=True)
parser.add_argument('--key', required=True)
parser.add_argument('--client-ca', required=True)
parser.add_argument('--port', type=int, default=47825)
parser.add_argument('--events')
parser.add_argument('--drop-first-pair-result', action='store_true')
parser.add_argument('--reject-first-pair-result', action='store_true')
parser.add_argument('--pair-drop-seconds',type=float,default=3)
parser.add_argument('--early-clipboard',action='store_true')
parser.add_argument('--recovery-files',action='store_true')
parser.add_argument('--witness-port',type=int)
parser.add_argument('--tls12-only',action='store_true')
parser.add_argument('--hello-drip',action='store_true')
parser.add_argument('--no-files',action='store_true')
parser.add_argument('--hello-delay',type=float,default=0)
parser.add_argument('--pair-drip',action='store_true')
parser.add_argument('--ready-close',action='store_true')
parser.add_argument('--reject-file-kind',action='store_true')
parser.add_argument('--stall-file-after-begin',action='store_true')
parser.add_argument('--unauthorized-hello',action='store_true')
parser.add_argument('--unpair-after-pair-result',action='store_true')
parser.add_argument('--drop-first-text-receipt',action='store_true')
parser.add_argument('--text-ack-delay',type=float,default=0)
parser.add_argument('--chunk-delay',type=float,default=0)
parser.add_argument('--first-connection-mode',choices=['wrong-pin','plaintext'])
parser.add_argument('--wrong-cert')
parser.add_argument('--wrong-key')
args = parser.parse_args()
write_lock = threading.Lock()
current = None
bound = False
pair_count = 0
last_clip = None
blocked_texts = set()
records = []
records_lock = threading.Lock()
fixture_expiry = None
text_dropped=False
release_text=threading.Event()
receive_ack_dropped=False
receive_metadata={}
receive_payloads={
    'photo': base64.b64decode('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aWQAAAABJRU5ErkJggg=='),
    'pdf': b'%PDF-1.4\nPneu received PDF fixture\n%%EOF\n',
}

def send_received_file(stream, name):
    assert args.receive_files and name in receive_payloads
    if name not in receive_metadata:
        data=receive_payloads[name]
        receive_metadata[name]=dict(transfer_id=('1' if name=='photo' else '2')*32,
            file_name='Pneu received fixture.'+('png' if name=='photo' else 'pdf'),
            mime='image/png' if name=='photo' else 'application/pdf',size=len(data),
            sha256=hashlib.sha256(data).hexdigest(),kind='photo' if name=='photo' else 'file',created_at=int(time.time()*1000))
        if args.mesh and name=='photo':receive_metadata[name].update(hlc=int(time.time()*1000),source_device='d'*32)
    meta=receive_metadata[name];data=receive_payloads[name]
    send(stream,0x10,**meta)
    chunk=bytes([0x11])+bytes.fromhex(meta['transfer_id'])+struct.pack('>Q',0)+data
    with write_lock:stream.sendall(struct.pack('>I',len(chunk))+chunk)
    if args.receive_progress_delay:time.sleep(args.receive_progress_delay)
    send(stream,0x12,transfer_id=meta['transfer_id'])
    event('SENT_TO_PHONE',transfer_id=meta['transfer_id'],kind=meta['kind'])


def witness_server():
    # Synthetic fixture control/evidence only; never used by the production app.
    global fixture_expiry
    with socket.create_server(('0.0.0.0',args.witness_port)) as listener:
        while True:
            connection,_=listener.accept()
            with connection:
                connection.settimeout(2)
                request=json.loads(connection.makefile('rb').readline(4096))
                if request.get('close_text'):release_text.set()
                if request.get('receive_file'):
                    assert current is not None, 'Phone must be connected'
                    send_received_file(current,request['receive_file'])
                if 'expires' in request:
                    fixture_expiry=request['expires'];event('INVITATION',expires=fixture_expiry)
                with records_lock:response=json.dumps(records).encode()+b'\n'
                connection.sendall(response)

def event(name, **fields):
    record={'event':name, 'at':int(time.time()*1000), **fields}
    with records_lock:records.append(record)
    if args.events:
        with open(args.events,'a') as output:
            output.write(json.dumps(record)+'\n')
    print(name, flush=True)

if args.witness_port:threading.Thread(target=witness_server,daemon=True).start()

def send_clipboard(stream):
    global last_clip
    last_clip=dict(transfer_id='e'*32,text='PC clipboard fixture',created_at=1791800000000,origin='clipboard')
    send(stream, 0x20, **last_clip)

def stop_on_command():
    for line in sys.stdin:
        command=line.strip()
        if command == 'quit':
            os._exit(0)
        elif command == 'clipboard' and current:
            send_clipboard(current)
        elif command == 'clipboard-repeat' and current and last_clip:
            send(current,0x20,**last_clip)
threading.Thread(target=stop_on_command, daemon=True).start()
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.minimum_version = context.maximum_version = ssl.TLSVersion.TLSv1_2 if args.tls12_only else ssl.TLSVersion.TLSv1_3
context.options |= ssl.OP_NO_TICKET
context.load_cert_chain(args.cert, args.key)
context.load_verify_locations(args.client_ca)
context.verify_mode = ssl.CERT_REQUIRED
wrong_context=None
if args.first_connection_mode=='wrong-pin':
    wrong_context=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    wrong_context.minimum_version=wrong_context.maximum_version=ssl.TLSVersion.TLSv1_3
    wrong_context.load_cert_chain(args.wrong_cert,args.wrong_key)
    wrong_context.load_verify_locations(args.client_ca)
    wrong_context.verify_mode=ssl.CERT_REQUIRED

def exact(stream, length):
    output = bytearray()
    while len(output) < length:
        chunk = stream.recv(length - len(output))
        if not chunk:
            raise EOFError()
        output.extend(chunk)
    return bytes(output)

def send(stream, wire_type, **payload):
    data = bytes([wire_type]) + json.dumps(payload).encode()
    with write_lock:
        stream.sendall(struct.pack('>I', len(data)) + data)

def recovery_file(stream, ident, file_kind, finish=True):
    data=b'recovery file bytes'
    fields=dict(transfer_id=ident*32,file_name='Recovery '+ident+'.bin',mime='image/png' if file_kind=='clipboard_image' else 'application/octet-stream',size=len(data),sha256=hashlib.sha256(data).hexdigest(),kind=file_kind,created_at=1000)
    if args.mesh and file_kind=='clipboard_image':fields.update(hlc=1000,source_device='d'*32)
    send(stream,0x10,**fields)
    chunk=bytes([0x11])+bytes.fromhex(ident*32)+struct.pack('>Q',0)+data[:5]
    with write_lock:stream.sendall(struct.pack('>I',len(chunk))+chunk)
    if finish:recovery_tail(stream,ident)

def recovery_tail(stream,ident):
    chunk=bytes([0x11])+bytes.fromhex(ident*32)+struct.pack('>Q',5)+b'recovery file bytes'[5:]
    with write_lock:stream.sendall(struct.pack('>I',len(chunk))+chunk)
    send(stream,0x12,transfer_id=ident*32)

with socket.create_server(('0.0.0.0', args.port)) as listener:
    print('READY', args.port, flush=True)
    accepted=0
    while True:
        raw, _ = listener.accept()
        accepted+=1
        if accepted==1 and args.first_connection_mode=='plaintext':
            event('BAD_TLS');raw.sendall(b'HTTP/1.0 400 Bad Request\r\n\r\n');raw.close();continue
        tls=None
        try:
            selected=wrong_context if accepted==1 and wrong_context is not None else context
            if selected is wrong_context and wrong_context is not None:event('WRONG_PIN')
            tls=selected.wrap_socket(raw,server_side=True,do_handshake_on_connect=False)
            try:tls.do_handshake()
            except ssl.SSLError:
                # Deliver the fatal TLS alert before closing. Closing immediately
                # with unread client bytes can replace it with a TCP reset.
                time.sleep(.5)
                raise
            with tls as stream:
                print('TLS', stream.version(), 'client_authenticated', flush=True)
                current = stream
                transfer = None
                while True:
                    size, = struct.unpack('>I', exact(stream, 4))
                    assert 1 <= size <= 262169
                    frame = exact(stream, size)
                    kind, body = frame[0], frame[1:]
                    if kind == 0x11:
                        assert transfer is not None
                        assert body[:16].hex() == transfer['transfer_id']
                        offset, = struct.unpack('>Q', body[16:24])
                        assert offset == transfer['count']
                        transfer['count'] += len(body) - 24
                        transfer['digest'].update(body[24:])
                        if args.chunk_delay:time.sleep(args.chunk_delay)
                        continue
                    payload = json.loads(body)
                    if kind == 1:
                        time.sleep(args.hello_delay)
                        if args.version_mismatch:
                            event('VERSION_MISMATCH');send(stream,127,code='version_mismatch',supported_version=2);break
                        if args.unauthorized_hello:
                            event('UNAUTHORIZED');send(stream,127,code='unauthorized');break
                        if args.hello_drip:
                            encoded=bytes([1])+json.dumps(dict(version=1,device_id='d'*32,name='Test PC',capabilities=['clipboard_text'])).encode()
                            for byte in struct.pack('>I',len(encoded))+encoded:
                                stream.send(bytes([byte]));time.sleep(.5)
                        else:
                            send(stream, 1, version=args.hello_version, device_id='d' * 32, name='Test PC', capabilities=(['future_feature'] if args.incompatible_capabilities else ['clipboard_text'] if args.no_files else ['files','clipboard_text','clipboard_image'])+(['mesh'] if args.mesh else []),**({'role':'computer'} if args.mesh else {}))
                            if args.early_clipboard and bound:send(stream,0x20,transfer_id='0'*32,text='Recovery clipboard fixture',created_at=1000,origin='clipboard')
                            if args.recovery_files and bound:
                                send(stream,4,nonce='9'*16)
                                send(stream,6,paused=False)
                                if args.mesh:
                                    phone=payload['device_id'];pc='d'*32
                                    send(stream,0x21,transfer_id='4'*32,settings=[dict(key=key,enabled=True,hlc=1000,writer_device=pc) for key in (phone+'/'+pc+'/clipboard',pc+'/'+phone+'/clipboard',phone+'/'+pc+'/automatic_media')])
                                send(stream,0x20,transfer_id='5'*32,text='Recovery clipboard',created_at=1000,origin='clipboard',**(dict(hlc=1000,source_device='d'*32) if args.mesh else {}))
                                recovery_file(stream,'6','file')
                                recovery_file(stream,'7','clipboard_image')
                                recovery_file(stream,'8','clipboard_image',finish=False)
                        event('HELLO',device_id=payload['device_id'],client_fp=hashlib.sha256(stream.getpeercert(binary_form=True)).hexdigest(),capabilities=payload['capabilities'])
                        if args.ready_close:time.sleep(.5);break
                    elif kind == 2:
                        # Synthetic harness invitation only; never use this for real pairing.
                        pair_count += 1
                        if args.reject_first_pair_result and pair_count == 1:
                            send(stream, 3, accepted=False, code='storage_failed')
                            event('PAIR_REJECTED', code='storage_failed')
                            break
                        bound=True
                        event('PAIR', attempt=pair_count)
                        if args.drop_first_pair_result and pair_count==1:
                            if fixture_expiry is not None:
                                assert int(time.time()*1000)<fixture_expiry, 'First request must arrive before expiry'
                                while int(time.time()*1000)<=fixture_expiry+250:time.sleep(.02)
                            else:time.sleep(args.pair_drop_seconds)
                            break
                        if args.pair_drip:
                            encoded=bytes([3])+json.dumps(dict(accepted=True)).encode()
                            for byte in struct.pack('>I',len(encoded))+encoded:
                                stream.send(bytes([byte]));time.sleep(.5)
                        else:send(stream, 3, accepted=True)
                        if args.recovery_files and pair_count>1:recovery_tail(stream,'8')
                        print('PAIR accepted', flush=True)
                        if args.unpair_after_pair_result:
                            time.sleep(1);send(stream,7);event('REMOTE_UNPAIR');break
                    elif kind == 4:
                        send(stream, 5, nonce=payload['nonce'])
                        print('PING/PONG', flush=True)
                    elif kind == 5:
                        event('PHONE_PONG',nonce=payload['nonce'])
                    elif kind == 6:
                        event('STATUS',paused=payload['paused'])
                    elif kind == 7:
                        event('UNPAIR')
                        bound=False;last_clip=None;break
                    elif kind == 16:
                        assert transfer is None
                        event('FILE_BEGIN',kind=payload['kind'])
                        if args.stall_file_after_begin:time.sleep(120)
                        transfer = payload | {'count': 0, 'digest': hashlib.sha256(), 'began': time.monotonic()}
                    elif kind == 18:
                        assert transfer['transfer_id'] == payload['transfer_id']
                        if transfer['count'] != transfer['size'] or transfer['digest'].hexdigest() != transfer['sha256']:
                            code='size_mismatch' if transfer['count']!=transfer['size'] else 'hash_mismatch'
                            send(stream,20,transfer_id=payload['transfer_id'],code=code,retryable=True)
                            event('FILE_REJECTED',code=code,size=transfer['count'])
                            transfer=None
                            continue
                        if args.reject_file_kind:
                            send(stream,20,transfer_id=payload['transfer_id'],code='unsupported_kind',retryable=False)
                            transfer=None
                            continue
                        send(stream, 19, transfer_id=payload['transfer_id'], deduplicated=False)
                        print('ACK', transfer['count'], 'bytes', round(time.monotonic()-transfer['began'], 3), 'seconds', flush=True)
                        event('FILE',transfer_id=transfer['transfer_id'],kind=transfer['kind'],size=transfer['count'],sha256=transfer['sha256'],mime=transfer['mime'])
                        transfer = None
                    elif kind == 0x20:
                        if args.drop_first_text_receipt and not text_dropped:
                            text_dropped=True
                            event('TEXT',transfer_id=payload['transfer_id'],created_at=payload['created_at'],origin=payload.get('origin','clipboard'),sha256=hashlib.sha256(payload['text'].encode()).hexdigest())
                            if not release_text.wait(20):raise TimeoutError('Fixture close not released')
                            event('DROPPED_TEXT',transfer_id=payload['transfer_id'])
                            break
                        if payload['text']=='Rejected older text fixture' and payload['transfer_id'] not in blocked_texts:
                            blocked_texts.add(payload['transfer_id'])
                            send(stream,0x14,transfer_id=payload['transfer_id'],code='id_conflict',retryable=False)
                            event('REJECTED_TEXT')
                            continue
                        if args.text_ack_delay:
                            def delayed_ack(target,ident):
                                time.sleep(args.text_ack_delay)
                                try:send(target,0x13,transfer_id=ident,deduplicated=False)
                                except (OSError,ValueError):pass
                            threading.Thread(target=delayed_ack,args=(stream,payload['transfer_id']),daemon=True).start()
                        else:send(stream,0x13,transfer_id=payload['transfer_id'],deduplicated=False)
                        clip_tests={'Request paused receive fixture':('f','Ignored while paused'),'Request disabled receive fixture':('c','Ignored while disabled'),'Request enabled receive fixture':('9','Enabled PC clipboard')}
                        if payload['text'] in clip_tests:
                            ident,text=clip_tests[payload['text']]
                            send(stream,0x20,transfer_id=ident*32,text=text,created_at=1791800000000,origin='clipboard')
                        if payload['text']=='Request replay fixture' and last_clip:
                            send(stream,0x20,**last_clip)
                        event('TEXT',transfer_id=payload['transfer_id'],created_at=payload['created_at'],origin=payload.get('origin','clipboard'),sha256=hashlib.sha256(payload['text'].encode()).hexdigest())
                        if payload['text']=='Share text fixture' and last_clip is None:
                            send_clipboard(stream)
                    elif kind == 0x21:
                        assert args.mesh
                        send(stream,0x13,transfer_id=payload['transfer_id'],deduplicated=False)
                        send(stream,0x21,transfer_id='3'*32,settings=payload['settings'])
                    elif kind == 0x13:
                        if args.receive_files and payload['transfer_id']=='1'*32 and not receive_ack_dropped:
                            receive_ack_dropped=True
                            event('DROPPED_FILE_ACK',transfer_id=payload['transfer_id'])
                            break
                        event('PHONE_ACK',transfer_id=payload['transfer_id'],deduplicated=payload['deduplicated'])
                    elif kind == 0x14:
                        event('PHONE_NACK',transfer_id=payload['transfer_id'],code=payload['code'],retryable=payload['retryable'])
                    elif kind == 127:
                        print('ERROR', payload['code'], flush=True)
                        break
                    else:
                        raise ValueError('unexpected frame')
        except (EOFError, ConnectionError, ssl.SSLError) as error:
            print(type(error).__name__, getattr(error, 'reason', ''), getattr(error, 'verify_message', ''), flush=True)
            raw.close()
        finally:
            if tls is not None:tls.close()
            current=None

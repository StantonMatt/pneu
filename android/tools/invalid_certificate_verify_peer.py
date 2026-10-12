#!/usr/bin/env python3
"""Test-only TLS 1.3 peer: correct pinned leaf, deliberately wrong CertificateVerify key.
No production code imports this fixture. Requires Python cryptography.
"""
import argparse, hashlib, hmac, os, socket, struct, threading, sys
from cryptography import x509
from cryptography.hazmat.primitives import serialization, hashes
from cryptography.hazmat.primitives.asymmetric import x25519, ec
from cryptography.hazmat.primitives.ciphers.aead import AESGCM
p=argparse.ArgumentParser();p.add_argument('--cert',required=True);p.add_argument('--port',type=int,default=47863);a=p.parse_args()
cert=x509.load_pem_x509_certificate(open(a.cert,'rb').read()).public_bytes(serialization.Encoding.DER)
def stop():
 for line in sys.stdin:
  if line.strip()=='quit':os._exit(0)
threading.Thread(target=stop,daemon=True).start()
def exact(s,n):
 b=b''
 while len(b)<n:
  part=s.recv(n-len(b))
  if not part:raise EOFError()
  b+=part
 return b
def record(s):
 header=exact(s,5);return header,exact(s,int.from_bytes(header[3:],'big'))
def hs(t,b):return bytes([t])+len(b).to_bytes(3,'big')+b
def extract(salt,ikm):return hmac.new(salt,ikm,hashlib.sha256).digest()
def expand(secret,label,context=b'',n=32):
 label=b'tls13 '+label;info=n.to_bytes(2,'big')+bytes([len(label)])+label+bytes([len(context)])+context
 return hmac.new(secret,info+b'\x01',hashlib.sha256).digest()[:n]
def nonce(iv,seq):return bytes(x^y for x,y in zip(iv,seq.to_bytes(12,'big')))
with socket.create_server(('0.0.0.0',a.port)) as listener:
 print('READY',a.port,flush=True)
 while True:
  raw,_=listener.accept()
  try:
   raw.settimeout(12);_,ch=record(raw);body=ch[4:];i=34;sid=body[i+1:i+1+body[i]];i+=1+body[i];i+=2+int.from_bytes(body[i:i+2],'big');i+=1+body[i];i+=2
   shares=[]
   while i<len(body):
    kind,size=struct.unpack('>HH',body[i:i+4]);value=body[i+4:i+4+size];i+=4+size
    if kind==51:
     j=2
     while j<len(value):
      group,length=struct.unpack('>HH',value[j:j+4]);shares.append((group,value[j+4:j+4+length]));j+=4+length
   group,client=next((g,b) for g,b in shares if g in (29,23))
   if group==29:
    key=x25519.X25519PrivateKey.generate();public=key.public_key().public_bytes(serialization.Encoding.Raw,serialization.PublicFormat.Raw);shared=key.exchange(x25519.X25519PublicKey.from_public_bytes(client))
   else:
    key=ec.generate_private_key(ec.SECP256R1());public=key.public_key().public_bytes(serialization.Encoding.X962,serialization.PublicFormat.UncompressedPoint);shared=key.exchange(ec.ECDH(),ec.EllipticCurvePublicKey.from_encoded_point(ec.SECP256R1(),client))
   ext=b'\x00\x2b\x00\x02\x03\x04'+struct.pack('>HHHH',51,4+len(public),group,len(public))+public
   sh=hs(2,b'\x03\x03'+os.urandom(32)+bytes([len(sid)])+sid+b'\x13\x01\x00'+len(ext).to_bytes(2,'big')+ext)
   raw.sendall(b'\x16\x03\x03'+len(sh).to_bytes(2,'big')+sh)
   transcript=ch+sh;empty=hashlib.sha256(b'').digest();secret=extract(expand(extract(bytes(32),bytes(32)),b'derived',empty),shared)
   digest=hashlib.sha256(transcript).digest();server=expand(secret,b's hs traffic',digest);client_secret=expand(secret,b'c hs traffic',digest)
   key=expand(server,b'key',n=16);iv=expand(server,b'iv',n=12)
   ee=hs(8,b'\x00\x00');entry=len(cert).to_bytes(3,'big')+cert+b'\x00\x00';certificate=hs(11,b'\x00'+len(entry).to_bytes(3,'big')+entry);transcript+=ee+certificate
   proof=b' '*64+b'TLS 1.3, server CertificateVerify\x00'+hashlib.sha256(transcript).digest()
   wrong=ec.generate_private_key(ec.SECP256R1()).sign(proof,ec.ECDSA(hashes.SHA256()))
   cv=hs(15,b'\x04\x03'+len(wrong).to_bytes(2,'big')+wrong);transcript+=cv
   finished=hs(20,hmac.new(expand(server,b'finished'),hashlib.sha256(transcript).digest(),hashlib.sha256).digest())
   plaintext=ee+certificate+cv+finished+b'\x16';header=b'\x17\x03\x03'+(len(plaintext)+16).to_bytes(2,'big');raw.sendall(header+AESGCM(key).encrypt(nonce(iv,0),plaintext,header));print('INVALID_CERTIFICATE_VERIFY_SENT',flush=True)
   while True:
    header,payload=record(raw)
    if header[0]==20:continue
    alert=AESGCM(expand(client_secret,b'key',n=16)).decrypt(nonce(expand(client_secret,b'iv',n=12),0),payload,header)
    print('CLIENT_ALERT',alert.hex(),flush=True);break
  except Exception as e:print(type(e).__name__,flush=True)
  finally:raw.close()

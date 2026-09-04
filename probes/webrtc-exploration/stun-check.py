"""One UDP STUN binding check. Print no addresses or transaction data."""
import json
import secrets
import socket
import struct

transaction = secrets.token_bytes(12)
request = struct.pack("!HHI", 1, 0, 0x2112A442) + transaction
with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp:
    udp.settimeout(4)
    try:
        udp.sendto(request, ("stun.l.google.com", 19302))
        response, _ = udp.recvfrom(2048)
        valid = (len(response) >= 20 and response[:2] == b"\x01\x01"
                 and response[4:8] == request[4:8] and response[8:20] == transaction)
        print(json.dumps({"udpStunBindingResponse": valid}))
        raise SystemExit(0 if valid else 1)
    except TimeoutError:
        print(json.dumps({"udpStunBindingResponse": False, "reason": "timeout"}))
        raise SystemExit(1)

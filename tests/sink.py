"""Where the browser's outbound TCP ends up: the firewall redirects every connection of the `browser` user here,
except to front and DNS (stack.nix). One JSON line per event on stdout: `connect` when a connection opens,
`request` with the request line and `purpose`, its Sec-Purpose and Sec-Fetch-Mode, when one arrives (prefetch
and navigate together: Chromium's NoStatePrefetch). TLS gets a throwaway certificate, which the browsers take
(WebDriver's acceptInsecureCerts), so an HTTPS fetch shows its request line too and a bare preconnect does not.

usage: python3 -I sink.py <port> <cert.pem> <key.pem>
"""

import json
import socket
import socketserver
import ssl
import struct
import sys
import threading

PORT, CERT, KEY = int(sys.argv[1]), sys.argv[2], sys.argv[3]
SO_ORIGINAL_DST = 80  # linux/netfilter_ipv4.h; the same number at SOL_IPV6 for IPv6


def original(sock):
    """Where the connection was going before the firewall's REDIRECT."""
    if sock.family == socket.AF_INET:
        raw = sock.getsockopt(socket.SOL_IP, SO_ORIGINAL_DST, 16)
        return f"{socket.inet_ntoa(raw[4:8])}:{struct.unpack('!H', raw[2:4])[0]}"
    raw = sock.getsockopt(socket.SOL_IPV6, SO_ORIGINAL_DST, 28)
    return f"[{socket.inet_ntop(socket.AF_INET6, raw[8:24])}]:{struct.unpack('!H', raw[2:4])[0]}"


def log(**event):
    print(json.dumps(event), flush=True)


class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        conn, dst, sni = self.request, original(self.request), None
        log(event="connect", dst=dst)
        conn.settimeout(10)
        try:
            if conn.recv(1, socket.MSG_PEEK) == b"\x16":  # a TLS handshake record
                names = []
                context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
                context.load_cert_chain(CERT, KEY)
                context.set_alpn_protocols(["http/1.1"])  # no HTTP/2, whose request is not a text line
                context.sni_callback = lambda _sock, name, _ctx: names.append(name)
                conn = context.wrap_socket(conn, server_side=True)
                sni = names[0] if names else None
            head = b""
            while b"\r\n\r\n" not in head and len(head) < 8192:  # the request line and its headers
                chunk = conn.recv(4096)
                if not chunk:
                    break
                head += chunk
            if b"\r\n" not in head:
                return
            line, *fields = head.split(b"\r\n\r\n", 1)[0].decode(errors="replace").split("\r\n")
            headers = {name.strip().lower(): value.strip() for name, _, value in (f.partition(":") for f in fields)}
            purpose = [headers.get("sec-purpose"), headers.get("sec-fetch-mode")]
            log(event="request", dst=dst, sni=sni, line=line, purpose=purpose)
            conn.sendall(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        except (OSError, ssl.SSLError):
            return


class Server(socketserver.ThreadingTCPServer):
    daemon_threads = True
    allow_reuse_address = True


class Server6(Server):
    address_family = socket.AF_INET6

    def server_bind(self):
        self.socket.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
        super().server_bind()


threading.Thread(target=Server6(("::", PORT), Handler).serve_forever, daemon=True).start()
Server(("0.0.0.0", PORT), Handler).serve_forever()

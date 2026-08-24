#!/usr/bin/env python3
"""Local CONNECT proxy that tunnels Telegram Bot API traffic to a reachable DC.

Why: on some networks the DNS-resolved api.telegram.org IP is blocked, while an
alternate Telegram data-center IP is reachable. The Telegram TLS certificate is
issued for the hostname `api.telegram.org`, so we must keep that hostname for
SNI/verification while routing the TCP connection to a different IP.

This proxy:
  - listens on a local port (default 1087)
  - accepts HTTP CONNECT `api.telegram.org:443`
  - opens a TCP connection to a configurable reachable IP (:py:data:`API_DC`)
  - relays bytes in both directions

Point jcode at it with:
  telegram_proxy = "http://127.0.0.1:1087"
or:
  JCODE_TELEGRAM_PROXY=http://127.0.0.1:1087

Usage:
  python3 scripts/telegram_dc_proxy.py [--listen 127.0.0.1:1087] [--to IP:port]
"""

import argparse
import socket
import sys
import threading

# Reachable Telegram data-center IP:port that answers for api.telegram.org:443.
# 149.154.167.220 verified working on networks where the default 149.154.166.110
# is blocked. Override with --to.
API_DC = "149.154.167.220:443"


def handle(client):
    """Relay one CONNECTed client connection to the reachable DC."""
    # Read the CONNECT request, preserving any early payload bytes that arrived
    # after the end of the header block (some clients pipeline the TLS
    # ClientHello before/with the CONNECT — dropping those corrupts TLS).
    buf = bytearray()
    while b"\r\n\r\n" not in buf:
        chunk = client.recv(4096)
        if not chunk:
            return
        buf += chunk
    head, sep, early = bytes(buf).partition(b"\r\n\r\n")

    parts = head.split(None, 2)
    if not parts or parts[0].upper() != b"CONNECT" or len(parts) < 2:
        print(f"[telegram-dc-proxy] REJECT 400 parts={parts}", flush=True)
        client.sendall(b"HTTP/1.1 400 Bad Request\r\n\r\n")
        client.close()
        return

    target = parts[1]  # e.g. api.telegram.org:443
    host = target.rsplit(b":", 1)[0]
    if not (host == b"api.telegram.org" or host.endswith(b".telegram.org")):
        print(f"[telegram-dc-proxy] REJECT 403 host={host!r}", flush=True)
        client.sendall(b"HTTP/1.1 403 Forbidden\r\n\r\n")
        client.close()
        return

    upstream_host, upstream_port = server_data.upstream.rsplit(":", 1)
    try:
        upstream = socket.create_connection((upstream_host, int(upstream_port)), timeout=20)
    except OSError as exc:
        print(f"[telegram-dc-proxy] upstream connect failed: {exc}", flush=True)
        client.sendall(b"HTTP/1.1 502 Bad Gateway\r\n\r\n")
        client.close()
        return

    print(f"[telegram-dc-proxy] CONNECT {target} -> {server_data.upstream}", flush=True)
    client.sendall(b"HTTP/1.1 200 Connection established\r\n\r\n")
    sys.stdout.flush()

    # Relay with correct half-close semantics. When one direction reaches EOF we
    # signal half-close on the peer only; only when both are done do we close.
    done = {"client": False, "upstream": False}

    def pump(src, dst, src_name):
        try:
            while True:
                d = src.recv(65536)
                if not d:
                    break
                dst.sendall(d)
        except OSError:
            pass
        finally:
            done[src_name] = True
            try:
                dst.shutdown(socket.SHUT_WR)
            except OSError:
                pass
            if done["client"] and done["upstream"]:
                for sock in (client, upstream):
                    try:
                        sock.close()
                    except OSError:
                        pass

    t1 = threading.Thread(target=pump, args=(client, upstream, "client"), daemon=True)
    t2 = threading.Thread(target=pump, args=(upstream, client, "upstream"), daemon=True)
    t1.start()
    t2.start()
    if early:
        # Preserve bytes that arrived before upstream was connected.
        try:
            upstream.sendall(early)
        except OSError:
            pass
    t1.join()
    t2.join()
    try:
        upstream.close()
    except OSError:
        pass


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--listen",
        default="127.0.0.1:1087",
        help="local listen address (default 127.0.0.1:1087)",
    )
    parser.add_argument(
        "--to",
        default=API_DC,
        help=f"reachable Telegram DC host:port (default {API_DC})",
    )
    args = parser.parse_args()

    listen_host, listen_port = args.listen.rsplit(":", 1)
    global server_data
    server_data = argparse.Namespace(upstream=args.to)

    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind((listen_host, int(listen_port)))
    listener.listen(16)

    print(
        f"[telegram-dc-proxy] listening on {args.listen} -> {args.to} "
        f"(CONNECT api.telegram.org:443)",
        flush=True,
    )
    try:
        while True:
            client, _ = listener.accept()
            threading.Thread(target=handle, args=(client,), daemon=True).start()
    except KeyboardInterrupt:
        pass
    finally:
        listener.close()


server_data = None

if __name__ == "__main__":
    main()
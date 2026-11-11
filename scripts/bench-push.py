#!/usr/bin/env python3
"""Many clients pushing to one repository at once, each to a branch of its
own: what a crowd of agents does. Speaks the push protocol directly, so the
numbers are the server's and not those of starting git thousands of times.

    bench-push.py <repository url> [--clients 32] [--seconds 20] [--with-listing] [--first 0]

Each push brings one commit with one changed file (three objects). With
--with-listing every push first asks for the list of refs, as git does.
Prints pushes per second, how long they took, and how many were refused.
To load several nodes at once, run one of these against each, with --first
set so that no two use the same branches.
"""
import argparse, hashlib, http.client, multiprocessing, struct, time, urllib.parse, zlib

WHO = b"Load <load@example.com> 1700000000 +0000"


def obj(kind: bytes, body: bytes):
    return hashlib.sha1(kind + b" %d\0" % len(body) + body).digest(), body


def entry(kind_bits: int, body: bytes) -> bytes:
    size, first = len(body), (kind_bits << 4) | (len(body) & 15)
    size >>= 4
    head = bytearray()
    while size:
        head.append(first | 0x80)
        first, size = size & 0x7F, size >> 7
    head.append(first)
    return bytes(head) + zlib.compress(body, 1)


def pack(objects) -> bytes:
    data = b"PACK" + struct.pack(">II", 2, len(objects)) + b"".join(entry(bits, body) for bits, body in objects)
    return data + hashlib.sha1(data).digest()


def commit_on(parent, client: int, number: int):
    """One commit changing one file, as the objects of a pack."""
    blob_id, blob = obj(b"blob", b"client %d, push %d\n" % (client, number))
    tree_id, tree = obj(b"tree", b"100644 file-%d.txt\0" % client + blob_id)
    lines = [b"tree " + tree_id.hex().encode()] + ([b"parent " + parent.hex().encode()] if parent else [])
    body = b"\n".join(lines + [b"author " + WHO, b"committer " + WHO, b"", b"push %d of client %d" % (number, client), b""])
    commit_id, commit = obj(b"commit", body)
    return commit_id, [(1, commit), (2, tree), (3, blob)]


def request(old, new, ref: str, objects) -> bytes:
    line = b"%s %s %s\0report-status\n" % ((old or bytes(20)).hex().encode(), new.hex().encode(), ref.encode())
    return b"%04x" % (len(line) + 4) + line + b"0000" + pack(objects)


def client(args):
    number, url, deadline, with_listing = args
    target = urllib.parse.urlparse(url)
    connection = http.client.HTTPConnection(target.netloc, timeout=60)
    ref, head, took, refused, count = f"refs/heads/load/{number}", None, [], 0, 0
    while time.time() < deadline:
        count += 1
        new, objects = commit_on(head, number, count)
        body = request(head, new, ref, objects)
        started = time.time()
        if with_listing:
            connection.request("GET", target.path + "/info/refs?service=git-receive-pack")
            connection.getresponse().read()
        connection.request("POST", target.path + "/git-receive-pack", body, {"content-type": "application/x-git-receive-pack-request"})
        answer = connection.getresponse().read()
        took.append(time.time() - started)
        if b"ok " + ref.encode() in answer:
            head = new
        else:
            refused += 1
    return took, refused


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("url")
    parser.add_argument("--clients", type=int, default=32)
    parser.add_argument("--seconds", type=float, default=20)
    parser.add_argument("--with-listing", action="store_true")
    parser.add_argument("--first", type=int, default=0, help="number of the first client, and of its branch")
    options = parser.parse_args()
    deadline = time.time() + options.seconds
    started = time.time()
    with multiprocessing.Pool(options.clients) as pool:
        results = pool.map(client, [(options.first + n, options.url, deadline, options.with_listing) for n in range(options.clients)])
    elapsed = time.time() - started
    took = sorted(t for times, _ in results for t in times)
    refused = sum(r for _, r in results)
    at = lambda q: took[min(len(took) - 1, int(len(took) * q))] * 1000
    print(
        f"{options.clients:4d} clients: {len(took) / elapsed:7.0f} pushes/s   "
        f"median {at(0.5):6.1f} ms   95% {at(0.95):6.1f} ms   99% {at(0.99):6.1f} ms   slowest {took[-1] * 1000:7.1f} ms   "
        f"{len(took)} pushes, {refused} refused"
    )


if __name__ == "__main__":
    main()

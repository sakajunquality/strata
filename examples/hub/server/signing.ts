// Signed commits. Strata hands over a commit's signature and the bytes it
// signs; whose key made it is the hub's to know. Users register the public
// keys they sign with (SSH or OpenPGP), and a commit is verified when its
// signature checks out against one of them.
import { createHash, createPublicKey, verify as checkSignature, type KeyObject } from "node:crypto";
import * as openpgp from "openpgp";
import { db, now } from "./db";

export type Signature = { format: string; signature: string; payload: string };
export type Verification = { state: "verified"; signer: string; key: string } | { state: "unverified"; reason: string } | null;

type KeyRow = { id: number; user_id: number; kind: "ssh" | "gpg"; title: string; material: string; fingerprint: string; created_at: number; user_name: string };
const KEYS = "SELECT signing_keys.*, owners.name AS user_name FROM signing_keys JOIN owners ON owners.id = signing_keys.user_id";

/** Reads the length-prefixed fields SSH formats are made of. */
class Fields {
  private at = 0;
  constructor(private bytes: Buffer) {}
  take(length: number): Buffer {
    if (length < 0 || this.at + length > this.bytes.length) throw new Error("truncated");
    this.at += length;
    return this.bytes.subarray(this.at - length, this.at);
  }
  number(): number {
    return this.take(4).readUInt32BE(0);
  }
  field(): Buffer {
    return this.take(this.number());
  }
}

const field = (bytes: Buffer | string) => {
  const data = Buffer.from(bytes);
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length);
  return Buffer.concat([length, data]);
};
const base64url = (bytes: Buffer) => bytes.toString("base64url");
/** A big number as SSH writes it, without the zero that keeps it positive. */
const unsigned = (bytes: Buffer) => (bytes.length > 1 && bytes[0] === 0 ? bytes.subarray(1) : bytes);
const CURVES: Record<string, { name: string; size: number; hash: string }> = {
  nistp256: { name: "P-256", size: 32, hash: "sha256" },
  nistp384: { name: "P-384", size: 48, hash: "sha384" },
  nistp521: { name: "P-521", size: 66, hash: "sha512" },
};

/** An SSH public key (the base64 part of a `.pub` line), ready to check signatures with. */
function sshKey(blob: Buffer): { type: string; key: KeyObject } {
  const fields = new Fields(blob);
  const type = fields.field().toString();
  if (type === "ssh-ed25519") return { type, key: createPublicKey({ format: "jwk", key: { kty: "OKP", crv: "Ed25519", x: base64url(fields.field()) } }) };
  if (type === "ssh-rsa") {
    const [e, n] = [fields.field(), fields.field()];
    return { type, key: createPublicKey({ format: "jwk", key: { kty: "RSA", e: base64url(unsigned(e)), n: base64url(unsigned(n)) } }) };
  }
  const curve = CURVES[type.replace("ecdsa-sha2-", "")];
  if (curve) {
    fields.field();
    const point = fields.field();
    if (point[0] !== 4 || point.length !== 1 + 2 * curve.size) throw new Error("unsupported point");
    const jwk = { kty: "EC", crv: curve.name, x: base64url(point.subarray(1, 1 + curve.size)), y: base64url(point.subarray(1 + curve.size)) };
    return { type, key: createPublicKey({ format: "jwk", key: jwk }) };
  }
  throw new Error(`${type} keys are not supported`);
}

const unarmored = (text: string) => Buffer.from(text.split("\n").filter((line) => !line.startsWith("-----")).join(""), "base64");

function verifySsh(signature: string, payload: Buffer): Verification {
  const fields = new Fields(unarmored(signature));
  if (fields.take(6).toString() !== "SSHSIG" || fields.number() !== 1) return { state: "unverified", reason: "has a signature in a format not known here" };
  const [publicKey, namespace, reserved, hash, signed] = [fields.field(), fields.field().toString(), fields.field(), fields.field().toString(), fields.field()];
  const row = db.query<KeyRow, [string]>(`${KEYS} WHERE signing_keys.kind = 'ssh' AND material = ?`).get(publicKey.toString("base64"));
  if (!row) return { state: "unverified", reason: "is signed with a key nobody has registered here" };
  // git signs in its own namespace, so a signature made for something else cannot be passed off as a commit's.
  if (namespace !== "git" || !["sha256", "sha512"].includes(hash)) return { state: "unverified", reason: "has a signature that was not made for git" };
  const message = Buffer.concat([Buffer.from("SSHSIG"), field(namespace), field(reserved), field(hash), field(createHash(hash).update(payload).digest())]);

  const { type, key } = sshKey(publicKey);
  const inner = new Fields(signed);
  const [algorithm, data] = [inner.field().toString(), inner.field()];
  let valid = false;
  if (type === "ssh-ed25519") valid = algorithm === type && checkSignature(null, message, key, data);
  else if (type === "ssh-rsa") valid = ["rsa-sha2-256", "rsa-sha2-512"].includes(algorithm) && checkSignature(algorithm.replace("rsa-sha2-", "sha"), message, key, data);
  else {
    const curve = CURVES[type.replace("ecdsa-sha2-", "")]!;
    const numbers = new Fields(data);
    const pad = (number: Buffer) => Buffer.concat([Buffer.alloc(Math.max(0, curve.size - unsigned(number).length)), unsigned(number)]);
    valid = algorithm === type && checkSignature(curve.hash, message, { key, dsaEncoding: "ieee-p1363" }, Buffer.concat([pad(numbers.field()), pad(numbers.field())]));
  }
  return valid ? { state: "verified", signer: row.user_name, key: row.fingerprint } : { state: "unverified", reason: "has a signature that does not check out" };
}

async function verifyGpg(armored: string, payload: Buffer): Promise<Verification> {
  const signature = await openpgp.readSignature({ armoredSignature: armored });
  for (const id of signature.getSigningKeyIDs()) {
    const row = db.query<KeyRow, [string]>(`${KEYS} WHERE signing_keys.kind = 'gpg' AND (' ' || key_ids || ' ') LIKE ?`).get(`% ${id.toHex()} %`);
    if (!row) continue;
    const result = await openpgp.verify({
      message: await openpgp.createMessage({ binary: new Uint8Array(payload) }),
      signature,
      verificationKeys: await openpgp.readKey({ armoredKey: row.material }),
    });
    const valid = await result.signatures[0]!.verified.catch(() => false);
    return valid ? { state: "verified", signer: row.user_name, key: row.fingerprint } : { state: "unverified", reason: "has a signature that does not check out" };
  }
  return { state: "unverified", reason: "is signed with a key nobody has registered here" };
}

/** Whether a commit's signature was made with a key registered here. Null for a commit that has none. */
export async function verify(signature: Signature | null | undefined): Promise<Verification> {
  if (!signature) return null;
  try {
    const payload = Buffer.from(signature.payload, "base64");
    if (signature.format === "ssh") return verifySsh(signature.signature, payload);
    if (signature.format === "openpgp") return await verifyGpg(signature.signature, payload);
    return { state: "unverified", reason: `has a ${signature.format} signature, which is not checked here` };
  } catch {
    return { state: "unverified", reason: "has a signature that cannot be read" };
  }
}

type Signed = { signature?: Signature | null };

/** Commits as the UI gets them: the signature replaced by what it turned out to be worth. */
export async function withVerification<T extends Signed>(commits: T[]): Promise<(Omit<T, "signature"> & { verification: Verification })[]> {
  return Promise.all(commits.map(async ({ signature, ...commit }) => ({ ...commit, verification: await verify(signature) })));
}

export function keysOf(userId: number) {
  return db
    .query<KeyRow, [number]>(`${KEYS} WHERE signing_keys.user_id = ? ORDER BY signing_keys.id DESC`)
    .all(userId)
    .map((row) => ({ id: row.id, kind: row.kind, title: row.title, fingerprint: row.fingerprint, createdAt: row.created_at }));
}

/** Registers a public key: a line as in `~/.ssh/id_ed25519.pub`, or an armored OpenPGP key. Throws a message for the user. */
export async function addKey(userId: number, title: string, raw: string) {
  const text = raw.trim();
  let key: { kind: "ssh" | "gpg"; material: string; fingerprint: string; ids: string };
  if (text.startsWith("-----BEGIN PGP PUBLIC KEY BLOCK")) {
    const parsed = await openpgp.readKey({ armoredKey: text }).catch(() => null);
    if (!parsed) throw new Error("that OpenPGP key cannot be read");
    const ids = parsed.getKeys().map((part) => part.getKeyID().toHex()).join(" ");
    key = { kind: "gpg", material: parsed.armor(), fingerprint: parsed.getFingerprint().toUpperCase(), ids };
  } else {
    const [type = "", encoded = ""] = text.split(/\s+/);
    const blob = Buffer.from(encoded, "base64");
    try {
      if (sshKey(blob).type !== type) throw new Error("mismatch");
    } catch {
      throw new Error("paste an SSH public key (ssh-ed25519, ssh-rsa or ecdsa) or an armored OpenPGP public key");
    }
    key = { kind: "ssh", material: blob.toString("base64"), fingerprint: `SHA256:${createHash("sha256").update(blob).digest("base64").replace(/=+$/, "")}`, ids: "" };
  }
  if (db.query("SELECT 1 FROM signing_keys WHERE kind = ? AND fingerprint = ?").get(key.kind, key.fingerprint)) throw new Error("that key is registered already");
  db.run("INSERT INTO signing_keys (user_id, kind, title, material, fingerprint, key_ids, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)", [userId, key.kind, title, key.material, key.fingerprint, key.ids, now()]);
}

export function removeKey(userId: number, id: number) {
  db.run("DELETE FROM signing_keys WHERE id = ? AND user_id = ?", [id, userId]);
}

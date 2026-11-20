import process from "node:process";
import { Buffer } from "node:buffer";
import { resolve as resolvePath } from "node:path";
import { connected, ask, workspace } from "node:internal/workspace";

const S_IFMT = 0o170000;
const S_IFDIR = 0o040000;
const S_IFREG = 0o100000;
const constants = {
  F_OK: 0,
  R_OK: 4,
  W_OK: 2,
  X_OK: 1,
  COPYFILE_EXCL: 1,
  COPYFILE_FICLONE: 2,
  COPYFILE_FICLONE_FORCE: 4,
  O_RDONLY: 0,
  O_WRONLY: 1,
  O_RDWR: 2,
  O_CREAT: 64,
  O_EXCL: 128,
  O_TRUNC: 512,
  O_APPEND: 1024,
  S_IFMT,
  S_IFREG,
  S_IFDIR,
  UV_DIRENT_FILE: 1,
  UV_DIRENT_DIR: 2,
};
const ERRNO = { ENOENT: -2, EBADF: -9, EEXIST: -17, ENOTDIR: -20, EISDIR: -21, EINVAL: -22, ENOSYS: -38, ENOTEMPTY: -39 };
const DESCRIPTIONS = {
  ENOENT: "no such file or directory",
  EBADF: "bad file descriptor",
  EEXIST: "file already exists",
  ENOTDIR: "not a directory",
  EISDIR: "illegal operation on a directory",
  EINVAL: "invalid argument",
  ENOSYS: "function not implemented",
  ENOTEMPTY: "directory not empty",
};

let inodes = 0;
const descriptors = new Map();
let nextDescriptor = 3;

function now() {
  return Date.now();
}

function directory(mode = 0o777, time = now()) {
  return { kind: "dir", entries: new Map(), mode: S_IFDIR | (mode & 0o7777 & ~0o022), ino: ++inodes, nlink: 2, atime: time, mtime: time, ctime: time, birthtime: time, made: epoch };
}

function file(data, mode = 0o666) {
  const time = now();
  return { kind: "file", data, mode: S_IFREG | (mode & 0o7777 & ~0o022), ino: ++inodes, nlink: 1, atime: time, mtime: time, ctime: time, birthtime: time, made: epoch };
}

// Each outermost fs call is one epoch; snapshot holds the workspace reads made during it.
let epoch = 0;
let calls = 0;
let snapshot;

const root = directory(0o755, 0);
root.entries.set("tmp", directory(0o1777, 0));
root.entries.set("work", directory(0o755, 0));

// With a connected workspace the tree mirrors it, and the workspace is the truth: other
// runtimes (a shell redirect, another cell) change it while a cell runs, so nothing here caches
// what it holds past one call. A directory looks each name up again (and lists again) on every
// call, and a file's bytes are read from the workspace each time they are needed and written to
// it at once when they change (see liveFile). Within one outermost fs call reads are remembered
// (see scoped), and a node this call made, or a name it removed, stands until the call writes it
// back (see mirrored).
function under(path, name) {
  return path === "/" ? "/" + name : path + "/" + name;
}


function scoped(fn) {
  return function (...args) {
    if (!connected()) return fn.apply(this, args);
    if (calls++ === 0) {
      epoch++;
      snapshot = new Map();
    }
    try {
      return fn.apply(this, args);
    } finally {
      if (--calls === 0) snapshot = undefined;
    }
  };
}

function fetchRemote(path) {
  let answer = snapshot?.get(path);
  if (answer === undefined) {
    const raw = ask({ op: "read", path });
    answer = raw.kind === "file" ? { kind: "file", bytes: new Uint8Array(Buffer.from(raw.data ?? "", "base64")) } : { kind: raw.kind === "dir" ? "dir" : "none" };
    snapshot?.set(path, answer);
  }
  return answer;
}

function listRemote(path) {
  const key = "\0list:" + path;
  let entries = snapshot?.get(key);
  if (entries === undefined) {
    entries = ask({ op: "list", path }).entries ?? [];
    snapshot?.set(key, entries);
  }
  return entries;
}

// Any change but a whole-file write drops what this call read.
function tell(request) {
  snapshot?.clear();
  return ask(request);
}

function writeRemote(path, bytes) {
  tell({ op: "write", path, data: Buffer.from(bytes).toString("base64") });
  snapshot?.set(path, { kind: "file", bytes });
}

// O_APPEND: the bytes go at the file's end as it is when they are written. A host whose info
// says append: true appends in one step; otherwise read, extend and write back.
function appendRemote(path, bytes) {
  if (workspace()?.append === true) {
    tell({ op: "append", path, data: Buffer.from(bytes).toString("base64") });
    return;
  }
  const answer = fetchRemote(path);
  writeRemote(path, concat(answer.kind === "file" ? answer.bytes : new Uint8Array(0), bytes));
}

function makeLive(node, path) {
  delete node.data;
  node.live = true;
  node.path = path;
  Object.defineProperty(node, "data", {
    configurable: true,
    enumerable: true,
    get() {
      const answer = fetchRemote(this.path);
      return answer.kind === "file" ? answer.bytes : new Uint8Array(0);
    },
    set(value) {
      writeRemote(this.path, value);
    },
  });
  return node;
}

function liveFile(path) {
  return makeLive(file(undefined), path);
}

function remoteDirectory(path, seed) {
  const node = directory(0o755);
  node.live = true;
  node.entries = new RemoteEntries(path, seed);
  return node;
}

// A node made (or a name removed) in this epoch and not yet written back stands; anything else
// is checked against the workspace.
function fresh(node) {
  return node !== undefined && !node.live && node.made === epoch;
}

class RemoteEntries extends Map {
  #gone = new Map();

  constructor(path, seed) {
    super();
    this.path = path;
    if (seed !== undefined) for (const [name, node] of seed) super.set(name, node);
  }

  #check(name) {
    if (typeof name !== "string" || this.#gone.get(name) === epoch) return;
    const cached = super.get(name);
    if (fresh(cached)) return;
    const path = under(this.path, name);
    const kind = fetchRemote(path).kind;
    if (kind === "file") {
      if (!(cached?.live && cached.kind === "file")) super.set(name, liveFile(path));
    } else if (kind === "dir") {
      if (!(cached?.live && cached.kind === "dir")) super.set(name, remoteDirectory(path));
    } else if (cached !== undefined) super.delete(name);
  }

  #list() {
    const listed = new Map(listRemote(this.path));
    for (const [name, node] of [...super.entries()]) {
      if (this.#gone.get(name) === epoch) continue;
      if (!listed.has(name) && !fresh(node)) super.delete(name);
    }
    for (const [name, kind] of listed) {
      if (this.#gone.get(name) === epoch) continue;
      const cached = super.get(name);
      if (fresh(cached) || (cached?.live && cached.kind === (kind === "dir" ? "dir" : "file"))) continue;
      const path = under(this.path, name);
      super.set(name, kind === "dir" ? remoteDirectory(path) : liveFile(path));
    }
  }

  // After a rename moved this directory: it and what it holds now live under path.
  relocate(path) {
    this.path = path;
    for (const [name, child] of super.entries()) relocate(child, under(path, name));
  }

  get(name) {
    this.#check(name);
    return super.get(name);
  }

  has(name) {
    this.#check(name);
    return super.has(name);
  }

  set(name, value) {
    this.#gone.delete(name);
    return super.set(name, value);
  }

  delete(name) {
    this.#gone.set(name, epoch);
    return super.delete(name);
  }

  get size() {
    this.#list();
    return super.size;
  }

  keys() {
    this.#list();
    return super.keys();
  }

  values() {
    this.#list();
    return super.values();
  }

  entries() {
    this.#list();
    return super.entries();
  }

  forEach(callback, self) {
    this.#list();
    return super.forEach(callback, self);
  }

  [Symbol.iterator]() {
    return this.entries();
  }
}

function relocate(node, path) {
  if (node.kind === "file") {
    if (node.live) node.path = path;
  } else if (node.entries instanceof RemoteEntries) node.entries.relocate(path);
}

let attached = false;

function attach() {
  if (attached || !connected()) return;
  attached = true;
  root.entries = new RemoteEntries("/");
}

function fsError(code, syscall, path, dest) {
  let message = code + ": " + DESCRIPTIONS[code] + ", " + syscall;
  if (path !== undefined) message += " '" + path + "'";
  if (dest !== undefined) message += " -> '" + dest + "'";
  const error = new Error(message);
  error.errno = ERRNO[code];
  error.code = code;
  error.syscall = syscall;
  if (path !== undefined) error.path = path;
  if (dest !== undefined) error.dest = dest;
  return error;
}

function codedError(Kind, code, message) {
  const error = new Kind(message);
  error.code = code;
  return error;
}

function unsupported(name) {
  return function () {
    const error = new Error("ENOSYS: function not implemented, fs." + name + " is not supported by the in-memory file system of Code Mode cells");
    error.errno = ERRNO.ENOSYS;
    error.code = "ENOSYS";
    error.syscall = name;
    throw error;
  };
}

function toPath(value, name = "path") {
  if (typeof value === "string") {
    if (value.includes("\0")) throw codedError(TypeError, "ERR_INVALID_ARG_VALUE", "The argument '" + name + "' must be a string, Uint8Array, or URL without null bytes. Received " + JSON.stringify(value));
    return value;
  }
  if (value instanceof Uint8Array) return Buffer.from(value).toString();
  if (value !== null && typeof value === "object" && typeof value.href === "string" && typeof value.protocol === "string") {
    if (value.protocol !== "file:") throw codedError(TypeError, "ERR_INVALID_URL_SCHEME", "The URL must be of scheme file");
    return decodeURIComponent(value.pathname);
  }
  throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", "The \"" + name + "\" argument must be of type string or an instance of Buffer or URL. Received " + (value === null ? "null" : typeof value));
}

function parts(absolute) {
  return absolute.split("/").filter((part) => part !== "" && part !== ".");
}

function lookup(path, syscall) {
  attach();
  const shown = toPath(path);
  const absolute = resolvePath(shown);
  let node = root;
  for (const part of parts(absolute)) {
    if (node.kind !== "dir") throw fsError("ENOTDIR", syscall, shown);
    node = node.entries.get(part);
    if (node === undefined) throw fsError("ENOENT", syscall, shown);
  }
  if (node.kind !== "dir" && /\/\.?$/.test(shown)) throw fsError("ENOTDIR", syscall, shown);
  return node;
}

function find(path) {
  try {
    return lookup(path, "stat");
  } catch {
    return undefined;
  }
}

function parentOf(path, syscall) {
  attach();
  const shown = toPath(path);
  const names = parts(resolvePath(shown));
  const name = names.pop();
  let node = root;
  for (const part of names) {
    if (node.kind !== "dir") throw fsError("ENOTDIR", syscall, shown);
    node = node.entries.get(part);
    if (node === undefined) throw fsError("ENOENT", syscall, shown);
  }
  if (node.kind !== "dir") throw fsError("ENOTDIR", syscall, shown);
  return { parent: node, name, shown };
}

function touch(node) {
  const time = now();
  node.mtime = time;
  node.ctime = time;
}

function encodingOf(options, fallback) {
  if (typeof options === "string") return options;
  if (options !== null && typeof options === "object" && options.encoding !== undefined) return options.encoding;
  return fallback;
}

function bytesOf(data, encoding) {
  if (typeof data === "string") return Buffer.from(data, encoding ?? "utf8");
  if (data instanceof Uint8Array) return Uint8Array.from(data);
  if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength));
  throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", "The \"data\" argument must be of type string or an instance of Buffer, TypedArray, or DataView. Received " + (data === null ? "null" : typeof data));
}

function decode(bytes, encoding) {
  const buffer = Buffer.from(bytes);
  return encoding === undefined || encoding === null || encoding === "buffer" ? buffer : buffer.toString(encoding);
}

class Stats {
  constructor(node) {
    this.dev = 1;
    this.mode = node.mode;
    this.nlink = node.nlink;
    this.uid = 0;
    this.gid = 0;
    this.rdev = 0;
    this.blksize = 4096;
    this.ino = node.ino;
    this.size = node.kind === "file" ? node.data.length : 4096;
    this.blocks = Math.ceil(this.size / 512);
    this.atimeMs = node.atime;
    this.mtimeMs = node.mtime;
    this.ctimeMs = node.ctime;
    this.birthtimeMs = node.birthtime;
    this.atime = new Date(node.atime);
    this.mtime = new Date(node.mtime);
    this.ctime = new Date(node.ctime);
    this.birthtime = new Date(node.birthtime);
  }

  isFile() {
    return (this.mode & S_IFMT) === S_IFREG;
  }

  isDirectory() {
    return (this.mode & S_IFMT) === S_IFDIR;
  }

  isSymbolicLink() {
    return false;
  }

  isBlockDevice() {
    return false;
  }

  isCharacterDevice() {
    return false;
  }

  isFIFO() {
    return false;
  }

  isSocket() {
    return false;
  }
}

class Dirent {
  constructor(name, node, parentPath) {
    this.name = name;
    this.parentPath = parentPath;
    this.path = parentPath;
    Object.defineProperty(this, "kind", { value: node.kind, enumerable: false });
  }

  isFile() {
    return this.kind === "file";
  }

  isDirectory() {
    return this.kind === "dir";
  }

  isSymbolicLink() {
    return false;
  }

  isBlockDevice() {
    return false;
  }

  isCharacterDevice() {
    return false;
  }

  isFIFO() {
    return false;
  }

  isSocket() {
    return false;
  }
}

function existsSync(path) {
  try {
    return find(path) !== undefined;
  } catch {
    return false;
  }
}

function accessSync(path) {
  lookup(path, "access");
}

function statSync(path, options) {
  let node;
  try {
    node = lookup(path, "stat");
  } catch (error) {
    if (options?.throwIfNoEntry === false && (error.code === "ENOENT" || error.code === "ENOTDIR")) return undefined;
    throw error;
  }
  return new Stats(node);
}

function lstatSync(path, options) {
  let node;
  try {
    node = lookup(path, "lstat");
  } catch (error) {
    if (options?.throwIfNoEntry === false && (error.code === "ENOENT" || error.code === "ENOTDIR")) return undefined;
    throw error;
  }
  return new Stats(node);
}

function descriptor(fd, syscall) {
  const entry = descriptors.get(fd);
  if (entry === undefined) throw fsError("EBADF", syscall);
  return entry;
}

function fstatSync(fd) {
  return new Stats(descriptor(fd, "fstat").node);
}

function readFileSync(path, options) {
  const encoding = encodingOf(options);
  if (typeof path === "number") {
    const entry = descriptor(path, "read");
    if (!entry.flags.read) throw fsError("EBADF", "read");
    const data = entry.node.data;
    const bytes = data.subarray(entry.position ?? data.length);
    entry.position = data.length;
    return decode(bytes, encoding);
  }
  const node = lookup(path, "open");
  if (node.kind === "dir") throw fsError("EISDIR", "read");
  node.atime = now();
  return decode(node.data, encoding);
}

function flagsOf(flag, fallback) {
  const value = flag ?? fallback;
  if (typeof value === "number") {
    return {
      read: (value & 3) !== 1,
      write: (value & 3) !== 0,
      create: (value & constants.O_CREAT) !== 0,
      exclusive: (value & constants.O_EXCL) !== 0,
      truncate: (value & constants.O_TRUNC) !== 0,
      append: (value & constants.O_APPEND) !== 0,
    };
  }
  const known = {
    r: [true, false, false, false, false, false],
    "r+": [true, true, false, false, false, false],
    rs: [true, false, false, false, false, false],
    "rs+": [true, true, false, false, false, false],
    w: [false, true, true, false, true, false],
    wx: [false, true, true, true, true, false],
    "w+": [true, true, true, false, true, false],
    "wx+": [true, true, true, true, true, false],
    a: [false, true, true, false, false, true],
    ax: [false, true, true, true, false, true],
    "a+": [true, true, true, false, false, true],
    "ax+": [true, true, true, true, false, true],
    as: [false, true, true, false, false, true],
    "as+": [true, true, true, false, false, true],
  }[String(value).replace("xs", "sx").replace(/^([rwa])s(x)?/, "$1$2")] ?? undefined;
  if (known === undefined) throw codedError(TypeError, "ERR_INVALID_ARG_VALUE", "The argument 'flags' is invalid. Received '" + value + "'");
  const [read, write, create, exclusive, truncate, append] = known;
  return { read, write, create, exclusive, truncate, append };
}

function openNode(path, flags, mode, syscall) {
  const { parent, name, shown } = parentOf(path, syscall);
  if (name === undefined) {
    if (flags.write) throw fsError("EISDIR", syscall, shown);
    return root;
  }
  let node = parent.entries.get(name);
  if (node !== undefined) {
    if (flags.create && flags.exclusive) throw fsError("EEXIST", syscall, shown);
    if (node.kind === "dir") {
      if (flags.write) throw fsError("EISDIR", syscall, shown);
      return node;
    }
    if (/\/\.?$/.test(shown)) throw fsError("ENOTDIR", syscall, shown);
    if (flags.truncate) {
      node.data = new Uint8Array(0);
      touch(node);
    }
    return node;
  }
  if (!flags.create) throw fsError("ENOENT", syscall, shown);
  if (shown.endsWith("/")) throw fsError("EISDIR", syscall, shown);
  node = file(new Uint8Array(0), typeof mode === "number" ? mode : 0o666);
  parent.entries.set(name, node);
  touch(parent);
  return node;
}

function writeFileSync(path, data, options) {
  const encoding = encodingOf(options, "utf8");
  const bytes = bytesOf(data, encoding);
  if (typeof path === "number") {
    writeAt(descriptor(path, "write"), bytes, null);
    return;
  }
  const flags = flagsOf(options?.flag, "w");
  // The new bytes replace the old in one write: no empty file in between for others to see.
  const node = openNode(path, { ...flags, truncate: false }, options?.mode, "open");
  if (node.kind === "dir") throw fsError("EISDIR", "open", toPath(path));
  if (flags.append) appendTo(node, bytes);
  else node.data = bytes;
  touch(node);
}

// Adds bytes at the end the file has now (a workspace file's, not one read earlier).
function appendTo(node, bytes) {
  if (node.live) appendRemote(node.path, bytes);
  else node.data = concat(node.data, bytes);
}

function appendFileSync(path, data, options) {
  const encoding = encodingOf(options, "utf8");
  const flag = typeof options === "object" && options?.flag !== undefined ? options.flag : "a";
  writeFileSync(path, bytesOf(data, encoding), { flag, mode: options?.mode });
}

function concat(a, b) {
  const out = new Uint8Array(a.length + b.length);
  out.set(a, 0);
  out.set(b, a.length);
  return out;
}

function mkdirSync(path, options) {
  const recursive = typeof options === "object" && options !== null && options.recursive === true;
  const mode = typeof options === "number" ? options : typeof options === "string" ? parseInt(options, 8) : options?.mode ?? 0o777;
  const shown = toPath(path);
  if (!recursive) {
    const { parent, name } = parentOf(path, "mkdir");
    if (name === undefined || parent.entries.has(name)) throw fsError("EEXIST", "mkdir", shown);
    parent.entries.set(name, directory(mode));
    parent.nlink++;
    touch(parent);
    return undefined;
  }
  attach();
  let node = root;
  let current = "";
  let first;
  for (const part of parts(resolvePath(shown))) {
    current += "/" + part;
    if (node.kind !== "dir") throw fsError("ENOTDIR", "mkdir", shown);
    let next = node.entries.get(part);
    if (next === undefined) {
      next = directory(mode);
      node.entries.set(part, next);
      node.nlink++;
      touch(node);
      first ??= current;
    }
    node = next;
  }
  if (node.kind !== "dir") throw fsError("EEXIST", "mkdir", shown);
  return first;
}

function mkdtempSync(prefix) {
  const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
  for (;;) {
    let suffix = "";
    for (let i = 0; i < 6; i++) suffix += alphabet[Math.floor(Math.random() * alphabet.length)];
    const path = toPath(prefix, "prefix") + suffix;
    if (!existsSync(path)) {
      mkdirSync(path, 0o700);
      return path;
    }
  }
}

function readdirSync(path, options) {
  const shown = toPath(path);
  const node = lookup(path, "scandir");
  if (node.kind !== "dir") throw fsError("ENOTDIR", "scandir", shown);
  const encoding = encodingOf(options, "utf8");
  const types = typeof options === "object" && options?.withFileTypes === true;
  const recursive = typeof options === "object" && options?.recursive === true;
  const out = [];
  const visit = (dir, prefix, parentPath) => {
    for (const name of [...dir.entries.keys()].sort()) {
      const child = dir.entries.get(name);
      const relative = prefix ? prefix + "/" + name : name;
      if (types) out.push(new Dirent(name, child, parentPath));
      else out.push(encoding === "buffer" ? Buffer.from(relative) : relative);
      if (recursive && child.kind === "dir") visit(child, relative, parentPath + "/" + name);
    }
  };
  visit(node, "", shown);
  return out;
}

function rmdirSync(path, options) {
  const { parent, name, shown } = parentOf(path, "rmdir");
  const node = name === undefined ? root : parent.entries.get(name);
  if (node === undefined) throw fsError("ENOENT", "rmdir", shown);
  if (node.kind !== "dir") throw fsError("ENOTDIR", "rmdir", shown);
  if (name === undefined) throw fsError("EINVAL", "rmdir", shown);
  if (node.entries.size > 0 && options?.recursive !== true) throw fsError("ENOTEMPTY", "rmdir", shown);
  parent.entries.delete(name);
  parent.nlink--;
  touch(parent);
}

function rmSync(path, options) {
  const shown = toPath(path);
  let parent;
  let name;
  try {
    ({ parent, name } = parentOf(path, "lstat"));
  } catch (error) {
    if (options?.force === true && (error.code === "ENOENT" || error.code === "ENOTDIR")) return;
    throw error;
  }
  const node = name === undefined ? root : parent.entries.get(name);
  if (node === undefined) {
    if (options?.force === true) return;
    throw fsError("ENOENT", "lstat", shown);
  }
  if (node.kind === "dir") {
    if (options?.recursive !== true) {
      const error = new Error("Path is a directory: rm returned EISDIR (is a directory) " + shown);
      error.name = "SystemError";
      error.code = "ERR_FS_EISDIR";
      error.errno = ERRNO.EISDIR;
      error.syscall = "rm";
      error.path = shown;
      throw error;
    }
    if (name === undefined) throw fsError("EINVAL", "rm", shown);
    parent.nlink--;
  }
  parent.entries.delete(name);
  touch(parent);
}

function unlinkSync(path) {
  const { parent, name, shown } = parentOf(path, "unlink");
  const node = name === undefined ? root : parent.entries.get(name);
  if (node === undefined) throw fsError("ENOENT", "unlink", shown);
  if (node.kind === "dir") throw fsError("EISDIR", "unlink", shown);
  parent.entries.delete(name);
  node.nlink--;
  touch(parent);
}

function contains(ancestor, node) {
  if (ancestor === node) return true;
  if (ancestor.kind !== "dir") return false;
  for (const child of ancestor.entries.values()) if (contains(child, node)) return true;
  return false;
}

function renameSync(from, to) {
  const source = parentOf(from, "rename");
  const target = parentOf(to, "rename");
  const node = source.name === undefined ? root : source.parent.entries.get(source.name);
  if (node === undefined) throw fsError("ENOENT", "rename", source.shown, target.shown);
  if (target.name === undefined) throw fsError("EISDIR", "rename", source.shown, target.shown);
  if (node.kind === "dir" && contains(node, target.parent)) throw fsError("EINVAL", "rename", source.shown, target.shown);
  const existing = target.parent.entries.get(target.name);
  if (existing !== undefined && existing !== node) {
    if (existing.kind === "dir" && node.kind !== "dir") throw fsError("EISDIR", "rename", source.shown, target.shown);
    if (existing.kind !== "dir" && node.kind === "dir") throw fsError("ENOTDIR", "rename", source.shown, target.shown);
    if (existing.kind === "dir" && existing.entries.size > 0) throw fsError("ENOTEMPTY", "rename", source.shown, target.shown);
  }
  source.parent.entries.delete(source.name);
  target.parent.entries.set(target.name, node);
  touch(source.parent);
  touch(target.parent);
  node.ctime = now();
}

function copyFileSync(from, to, mode = 0) {
  const shownFrom = toPath(from);
  const node = lookup(from, "copyfile");
  if (node.kind === "dir") throw fsError("EISDIR", "copyfile", shownFrom, toPath(to));
  const { parent, name, shown } = parentOf(to, "copyfile");
  const existing = parent.entries.get(name);
  if (existing !== undefined && (mode & constants.COPYFILE_EXCL)) throw fsError("EEXIST", "copyfile", shownFrom, shown);
  if (existing?.kind === "dir") throw fsError("EISDIR", "copyfile", shownFrom, shown);
  if (existing !== undefined) {
    existing.data = Uint8Array.from(node.data);
    touch(existing);
  } else {
    parent.entries.set(name, file(Uint8Array.from(node.data), node.mode));
    touch(parent);
  }
}

function cpSync(from, to, options) {
  const node = lookup(from, "cp");
  if (node.kind === "dir") {
    if (options?.recursive !== true) {
      const error = new Error("Recursive option is required to copy a directory: " + toPath(from));
      error.name = "SystemError";
      error.code = "ERR_FS_EISDIR";
      throw error;
    }
    if (!existsSync(to)) mkdirSync(to, { recursive: true });
    for (const name of node.entries.keys()) cpSync(toPath(from) + "/" + name, toPath(to) + "/" + name, options);
    return;
  }
  if (existsSync(to) && options?.force === false) {
    if (options?.errorOnExist) throw fsError("EEXIST", "cp", toPath(to));
    return;
  }
  copyFileSync(from, to);
}

function truncateSync(path, length = 0) {
  if (typeof path === "number") return ftruncateSync(path, length);
  const node = lookup(path, "open");
  if (node.kind === "dir") throw fsError("EISDIR", "open", toPath(path));
  resize(node, length);
}

function resize(node, length) {
  const out = new Uint8Array(length);
  out.set(node.data.subarray(0, length));
  node.data = out;
  touch(node);
}

function ftruncateSync(fd, length = 0) {
  resize(descriptor(fd, "ftruncate").node, length);
}

function timeOf(value) {
  if (value instanceof Date) return value.getTime();
  if (typeof value === "string") return Number(value) * 1000;
  return value * 1000;
}

function utimesSync(path, atime, mtime) {
  const node = lookup(path, "utime");
  node.atime = timeOf(atime);
  node.mtime = timeOf(mtime);
}

function futimesSync(fd, atime, mtime) {
  const node = descriptor(fd, "futime").node;
  node.atime = timeOf(atime);
  node.mtime = timeOf(mtime);
}

function chmodSync(path, mode) {
  const node = lookup(path, "chmod");
  node.mode = (node.mode & S_IFMT) | ((typeof mode === "string" ? parseInt(mode, 8) : mode) & 0o7777);
  node.ctime = now();
}

function fchmodSync(fd, mode) {
  const node = descriptor(fd, "fchmod").node;
  node.mode = (node.mode & S_IFMT) | ((typeof mode === "string" ? parseInt(mode, 8) : mode) & 0o7777);
}

function chownSync(path) {
  lookup(path, "chown");
}

function realpathSync(path) {
  lookup(path, "realpath");
  return resolvePath(toPath(path));
}
realpathSync.native = realpathSync;

function readlinkSync(path) {
  lookup(path, "readlink");
  throw fsError("EINVAL", "readlink", toPath(path));
}

function linkSync(existing, path) {
  const node = lookup(existing, "link");
  if (node.kind === "dir") throw Object.assign(fsError("EISDIR", "link", toPath(existing), toPath(path)), { code: "EPERM", errno: -1 });
  const { parent, name, shown } = parentOf(path, "link");
  if (parent.entries.has(name)) throw fsError("EEXIST", "link", toPath(existing), shown);
  parent.entries.set(name, node);
  node.nlink++;
  touch(parent);
}

function openSync(path, flags = "r", mode) {
  const parsed = flagsOf(flags, "r");
  const node = openNode(path, parsed, mode, "open");
  const fd = nextDescriptor++;
  descriptors.set(fd, { node, position: 0, flags: parsed, path: toPath(path), absolute: resolvePath(toPath(path)) });
  return fd;
}

function closeSync(fd) {
  descriptor(fd, "close");
  descriptors.delete(fd);
}

function fsyncSync(fd) {
  descriptor(fd, "fsync");
}

function readSync(fd, buffer, offsetOrOptions, length, position) {
  const entry = descriptor(fd, "read");
  let offset = offsetOrOptions;
  if (offsetOrOptions !== null && typeof offsetOrOptions === "object") {
    ({ offset = 0, length = buffer.byteLength - offset, position = null } = offsetOrOptions);
  }
  offset ??= 0;
  length ??= buffer.byteLength - offset;
  if (entry.node.kind === "dir") throw fsError("EISDIR", "read");
  if (!entry.flags.read) throw fsError("EBADF", "read");
  const data = entry.node.data;
  const positioned = typeof position === "number" || typeof position === "bigint";
  const start = positioned ? Number(position) : entry.position ?? data.length;
  const bytes = data.subarray(start, start + length);
  new Uint8Array(buffer.buffer, buffer.byteOffset, buffer.byteLength).set(bytes, offset);
  if (!positioned) entry.position = start + bytes.length;
  return bytes.length;
}

function writeAt(entry, bytes, position) {
  if (!entry.flags.write) throw fsError("EBADF", "write");
  const node = entry.node;
  if (entry.flags.append) {
    // O_APPEND: every write lands at the end the file has when it is made (a position is
    // ignored, as on Linux), and the offset moves to that end.
    appendTo(node, bytes);
    entry.position = null;
    touch(node);
    return bytes.length;
  }
  const start = typeof position === "number" ? position : entry.position ?? node.data.length;
  if (node.live) {
    // Over what the workspace holds now: bytes others wrote around this range stay.
    const current = node.data;
    const out = new Uint8Array(Math.max(current.length, start + bytes.length));
    out.set(current);
    out.set(bytes, start);
    node.data = out;
  } else {
    if (start + bytes.length > node.data.length) {
      const grown = new Uint8Array(start + bytes.length);
      grown.set(node.data);
      node.data = grown;
    }
    node.data.set(bytes, start);
  }
  if (typeof position !== "number") entry.position = start + bytes.length;
  touch(node);
  return bytes.length;
}

function writeSync(fd, data, offsetOrPosition, lengthOrEncoding, position) {
  const entry = descriptor(fd, "write");
  if (typeof data === "string") {
    return writeAt(entry, Buffer.from(data, typeof lengthOrEncoding === "string" ? lengthOrEncoding : "utf8"), typeof offsetOrPosition === "number" ? offsetOrPosition : null);
  }
  const bytes = new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  const offset = offsetOrPosition ?? 0;
  const length = typeof lengthOrEncoding === "number" ? lengthOrEncoding : bytes.length - offset;
  return writeAt(entry, bytes.subarray(offset, offset + length), position);
}

// Writes a change made through this module back to the connected workspace. Calls made inside
// another fs call (cpSync copying files, rmSync removing them) leave it to the outermost one.
let depth = 0;

function mirrored(fn, after) {
  return function (...args) {
    if (!connected()) return fn.apply(this, args);
    attach();
    const before = after.length > 2 ? args.map((arg) => (typeof arg === "number" ? descriptors.get(arg) : undefined)) : undefined;
    depth++;
    let result;
    try {
      result = fn.apply(this, args);
    } finally {
      depth--;
    }
    if (depth === 0) after(args, result, before);
    return result;
  };
}

// A file that is live at this path already wrote every change through; one this call made is
// written once and turns live in place (so open descriptors follow the workspace from then on).
function mirror(path, deep = false) {
  const absolute = resolvePath(path);
  const node = find(absolute);
  if (node === undefined) tell({ op: "remove", path: absolute });
  else if (node.kind === "dir") {
    tell({ op: "mkdir", path: absolute });
    liven(absolute);
    if (deep) for (const name of node.entries.keys()) mirror(under(absolute, name), true);
  } else if (!(node.live && node.path === absolute)) {
    writeRemote(absolute, node.data);
    if (node.live) {
      // A hard link: the workspace holds a copy, which this path now follows.
      const { parent, name } = parentOf(absolute, "link");
      parent.entries.set(name, liveFile(absolute));
    } else makeLive(node, absolute);
  }
}

// Directories made on the way to absolute now follow the workspace.
function liven(absolute) {
  let node = root;
  let path = "/";
  for (const part of parts(absolute)) {
    path = under(path, part);
    const next = node.entries.get(part);
    if (next === undefined || next.kind !== "dir") return;
    if (!next.live) {
      next.live = true;
      next.entries = new RemoteEntries(path, next.entries);
    }
    node = next;
  }
}

function renamed(from, to) {
  const target = resolvePath(toPath(to));
  tell({ op: "rename", from: resolvePath(toPath(from)), to: target });
  const node = find(target);
  if (node !== undefined) relocate(node, target);
}

function mirrorTarget(target, entry) {
  if (typeof target === "number") {
    entry ??= descriptors.get(target);
    if (entry?.flags.write) mirror(entry.absolute);
  } else mirror(toPath(target));
}

writeFileSync = mirrored(writeFileSync, ([target]) => mirrorTarget(target));
appendFileSync = mirrored(appendFileSync, ([target]) => mirrorTarget(target));
truncateSync = mirrored(truncateSync, ([target]) => mirrorTarget(target));
openSync = mirrored(openSync, (_, fd) => mirrorTarget(fd));
mkdirSync = mirrored(mkdirSync, ([path]) => mirror(toPath(path)));
mkdtempSync = mirrored(mkdtempSync, (_, path) => mirror(path));
rmdirSync = mirrored(rmdirSync, ([path]) => mirror(toPath(path)));
rmSync = mirrored(rmSync, ([path]) => mirror(toPath(path)));
unlinkSync = mirrored(unlinkSync, ([path]) => mirror(toPath(path)));
renameSync = mirrored(renameSync, ([from, to]) => renamed(from, to));
copyFileSync = mirrored(copyFileSync, ([, to]) => mirror(toPath(to)));
cpSync = mirrored(cpSync, ([, to]) => mirror(toPath(to), true));
linkSync = mirrored(linkSync, ([, path]) => mirror(toPath(path)));
// Writes through descriptors (writeSync, ftruncateSync) reach a workspace file as they are made,
// so fsyncSync and closeSync have nothing left to write back. Every call reads the workspace
// afresh (see scoped).
existsSync = scoped(existsSync);
accessSync = scoped(accessSync);
statSync = scoped(statSync);
lstatSync = scoped(lstatSync);
fstatSync = scoped(fstatSync);
readFileSync = scoped(readFileSync);
writeFileSync = scoped(writeFileSync);
appendFileSync = scoped(appendFileSync);
mkdirSync = scoped(mkdirSync);
mkdtempSync = scoped(mkdtempSync);
readdirSync = scoped(readdirSync);
rmdirSync = scoped(rmdirSync);
rmSync = scoped(rmSync);
unlinkSync = scoped(unlinkSync);
renameSync = scoped(renameSync);
copyFileSync = scoped(copyFileSync);
cpSync = scoped(cpSync);
truncateSync = scoped(truncateSync);
ftruncateSync = scoped(ftruncateSync);
utimesSync = scoped(utimesSync);
futimesSync = scoped(futimesSync);
chmodSync = scoped(chmodSync);
fchmodSync = scoped(fchmodSync);
chownSync = scoped(chownSync);
realpathSync = scoped(realpathSync);
readlinkSync = scoped(readlinkSync);
linkSync = scoped(linkSync);
openSync = scoped(openSync);
readSync = scoped(readSync);
writeSync = scoped(writeSync);
realpathSync.native = realpathSync;

const sync = {
  existsSync,
  accessSync,
  statSync,
  lstatSync,
  fstatSync,
  readFileSync,
  writeFileSync,
  appendFileSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  rmdirSync,
  rmSync,
  unlinkSync,
  renameSync,
  copyFileSync,
  cpSync,
  truncateSync,
  ftruncateSync,
  utimesSync,
  lutimesSync: utimesSync,
  futimesSync,
  chmodSync,
  lchmodSync: chmodSync,
  fchmodSync,
  chownSync,
  lchownSync: chownSync,
  fchownSync: (fd) => void descriptor(fd, "fchown"),
  realpathSync,
  readlinkSync,
  linkSync,
  symlinkSync: unsupported("symlinkSync"),
  openSync,
  closeSync,
  fsyncSync,
  fdatasyncSync: fsyncSync,
  readSync,
  writeSync,
};

function callbackOf(args, name) {
  const callback = args.pop();
  if (typeof callback !== "function") {
    throw codedError(TypeError, "ERR_INVALID_ARG_TYPE", "The \"cb\" argument must be of type function. Received " + (callback === null ? "null" : typeof callback));
  }
  return callback;
}

const callbacks = {};
for (const [name, fn] of Object.entries(sync)) {
  if (name === "existsSync") continue;
  const plain = name.slice(0, -4);
  callbacks[plain] = function (...args) {
    const callback = callbackOf(args, plain);
    let result;
    let failure = null;
    try {
      result = fn(...args);
    } catch (error) {
      failure = error;
    }
    queueMicrotask(() => (failure ? callback(failure) : plain === "read" || plain === "write" ? callback(null, result, args[1]) : callback(null, result)));
  };
}
callbacks.exists = function (path, callback) {
  const result = existsSync(path);
  queueMicrotask(() => callback(result));
};
callbacks.realpath.native = callbacks.realpath;

class FileHandle {
  #fd;

  constructor(fd) {
    this.#fd = fd;
  }

  get fd() {
    return this.#fd;
  }

  async readFile(options) {
    return readFileSync(this.#fd, options);
  }

  async writeFile(data, options) {
    return writeFileSync(this.#fd, data, options);
  }

  async appendFile(data, options) {
    return writeSync(this.#fd, bytesOf(data, encodingOf(options, "utf8")));
  }

  async read(buffer, offset, length, position) {
    buffer ??= Buffer.alloc(16384);
    const bytesRead = readSync(this.#fd, buffer, offset, length, position);
    return { bytesRead, buffer };
  }

  async write(data, ...rest) {
    const bytesWritten = writeSync(this.#fd, data, ...rest);
    return { bytesWritten, buffer: data };
  }

  async stat() {
    return fstatSync(this.#fd);
  }

  async truncate(length) {
    return ftruncateSync(this.#fd, length);
  }

  async sync() {
    return fsyncSync(this.#fd);
  }

  async close() {
    return closeSync(this.#fd);
  }

  async [Symbol.asyncDispose]() {
    if (descriptors.has(this.#fd)) closeSync(this.#fd);
  }
}

const promises = {};
for (const [name, fn] of Object.entries(sync)) {
  if (name === "existsSync" || name === "openSync" || name === "closeSync" || name === "readSync" || name === "writeSync" || name.startsWith("f")) continue;
  const plain = name.slice(0, -4);
  promises[plain] = async (...args) => fn(...args);
}
promises.open = async (path, flags, mode) => new FileHandle(openSync(path, flags, mode));
promises.constants = constants;

const createReadStream = unsupported("createReadStream");
const createWriteStream = unsupported("createWriteStream");
const watch = unsupported("watch");
const watchFile = unsupported("watchFile");
const unwatchFile = unsupported("unwatchFile");
const opendirSync = unsupported("opendirSync");
const statfsSync = unsupported("statfsSync");

const fs = {
  ...sync,
  ...callbacks,
  promises,
  constants,
  Stats,
  Dirent,
  createReadStream,
  createWriteStream,
  watch,
  watchFile,
  unwatchFile,
  opendirSync,
  statfsSync,
  F_OK: constants.F_OK,
  R_OK: constants.R_OK,
  W_OK: constants.W_OK,
  X_OK: constants.X_OK,
};

export default fs;
export { promises, constants, Stats, Dirent, FileHandle, createReadStream, createWriteStream, watch, watchFile, unwatchFile, opendirSync, statfsSync };
export { existsSync, accessSync, statSync, lstatSync, fstatSync, readFileSync, writeFileSync, appendFileSync, mkdirSync, mkdtempSync, readdirSync, rmdirSync, rmSync, unlinkSync, renameSync, copyFileSync, cpSync, truncateSync, ftruncateSync, utimesSync, futimesSync, chmodSync, fchmodSync, chownSync, realpathSync, readlinkSync, linkSync, openSync, closeSync, fsyncSync, readSync, writeSync };
export const { lutimesSync, lchmodSync, lchownSync, fchownSync, symlinkSync, fdatasyncSync } = sync;
export const { access, stat, lstat, fstat, readFile, writeFile, appendFile, mkdir, mkdtemp, readdir, rmdir, rm, unlink, rename, copyFile, cp, truncate, ftruncate, utimes, futimes, chmod, fchmod, chown, realpath, readlink, link, open, close, fsync, read, write, lutimes, lchmod, lchown, fchown, symlink, fdatasync, exists } = callbacks;

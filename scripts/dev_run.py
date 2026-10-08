#!/usr/bin/env python3
"""Update the verified local Windows dev HUD, using its existing restart API."""
from __future__ import annotations

import argparse
import base64
from contextlib import contextmanager
import hashlib
import importlib.util
import json
import math
import ntpath
import shutil
import signal
import struct
import subprocess
import sys
import time
import threading
import urllib.error
import urllib.parse
import urllib.request
import uuid
from pathlib import Path


class DevRunError(Exception):
    """A sanitized error suitable for the CLI."""


class Rejected(DevRunError):
    """The runtime definitively rejected restart before admission."""


class AccessDenied(DevRunError):
    """Do not continue polling an unauthorized endpoint."""


class FileOutcomeUnknown(DevRunError):
    """A timed-out interop writer may still be active; do not race a rollback."""


class HTTPDeadline(DevRunError):
    """The synchronous request exceeded its wall-clock budget."""


@contextmanager
def request_wall_deadline(seconds):
    """WSL/main-thread guard; never borrow an already active process alarm."""
    if (threading.current_thread() is not threading.main_thread()
            or not hasattr(signal, "setitimer") or not hasattr(signal, "ITIMER_REAL")):
        raise Rejected("HTTP wall-clock guard unavailable; request not sent")
    if not math.isfinite(seconds) or seconds <= 0:
        raise Rejected("HTTP deadline expired; request not sent")
    handler = signal.getsignal(signal.SIGALRM)
    timer = signal.getitimer(signal.ITIMER_REAL)
    if timer != (0.0, 0.0):
        # Keep another controller/client's handler and running timer untouched.
        raise Rejected("process alarm already active; request not sent")

    def expire(signum, frame):
        raise HTTPDeadline("HTTP wall-clock deadline expired; outcome unproved")

    try:
        signal.signal(signal.SIGALRM, expire)
        signal.setitimer(signal.ITIMER_REAL, seconds)
        yield
    finally:
        try:
            signal.setitimer(signal.ITIMER_REAL, 0)
        finally:
            try:
                signal.signal(signal.SIGALRM, handler)
            finally:
                signal.setitimer(signal.ITIMER_REAL, *timer)


def windows_path(value: str) -> str:
    if not isinstance(value, str) or not value or not ntpath.isabs(value):
        raise DevRunError("unknown Windows path identity")
    return ntpath.normcase(ntpath.normpath(value))


def validate(status: dict, process: dict, *, approved_image: str | None = None) -> None:
    """Native canonical paths, file IDs and listener ownership come from the bridge."""
    if (type(status.get("pid")) is not int or status["pid"] <= 0
            or not isinstance(status.get("sha"), str) or len(status["sha"]) != 40
            or any(c not in "0123456789abcdef" for c in status["sha"])
            or not isinstance(status.get("channel"), str)
            or process.get("pid") != status["pid"] or not process.get("created")
            or process.get("local_listener_verified") is not True
            or process.get("canonical_verified") is not True
            or not process.get("file_id") or not process.get("directory_id")):
        raise DevRunError("HUD status/local Windows process identity is unproved")
    image = windows_path(process.get("image"))
    production = windows_path(process.get("production"))
    dev = windows_path(process.get("dev_dir"))
    if (image == production or process.get("production_file_id") == process["file_id"]):
        raise DevRunError("refusing the installed production HUD, including channel ci")
    if ntpath.basename(image) != "tze_hud.exe":
        raise DevRunError("the listener does not belong to tze_hud.exe")
    if approved_image is not None:
        if image != approved_image:
            raise DevRunError("restarted image differs from the approved target")
    elif status["channel"] != "ci":
        try:
            within_dev = ntpath.commonpath([image, dev]) == dev
        except ValueError:
            within_dev = False
        if not within_dev:
            raise DevRunError("HUD must be channel ci or inside the selected dev directory")
    cpu = status.get("cpu_pct_2s")
    if cpu is not None and (type(cpu) not in (int, float) or not math.isfinite(cpu)):
        raise DevRunError("malformed HUD CPU sample")


def process_key(process: dict) -> tuple:
    return (process["pid"], process["created"], windows_path(process["image"]),
            process["file_id"], process["directory_id"])


def verify_same_process(ops, expected):
    status = ops.status(8)
    process = ops.process(status["pid"], 10)
    validate(status, process)
    if process_key(process) != tuple(expected):
        raise DevRunError("HUD PID/creation/image identity changed before parking")


class Transaction:
    """The one executable slot; injected operations also exercise actual rollback."""

    def __init__(self, ops, process: dict):
        self.ops = ops
        self.state = {"schema": 1, "token": ops.token, "image": windows_path(process["image"]),
                      "old_process": process_key(process), "phase": "locked"}
        self.post_started = False

    def begin(self):
        self.ops.lock()
        try:
            old = self.ops.file("old")
            receipt = self.ops.receipt()
            if old is not None:
                current = self.ops.file("exe")
                if (not receipt or receipt.get("schema") != 1
                        or receipt.get("phase") != "complete"
                        or receipt.get("image") != self.state["image"]
                        or old != receipt.get("old_file")
                        or current != receipt.get("new_file")):
                    raise DevRunError("unowned or unresolved tze_hud.old.exe; inspect it manually")
                self.ops.remove("old", old)
            elif receipt and (receipt.get("schema") != 1 or receipt.get("phase") != "complete"
                              or receipt.get("image") != self.state["image"]):
                raise DevRunError("unrecognized previous dev-run receipt")
            self.state["old_file"] = self.ops.file("exe")
            if self.state["old_file"] is None:
                raise DevRunError("verified executable disappeared")
            self.save()
        except Exception:
            self.ops.unlock()
            raise

    def save(self):
        self.ops.journal(self.state)

    def swap(self, build: dict):
        self.state["new_file"] = self.ops.copy_stage(build)
        if self.state["new_file"]["sha256"] != build["sha256"]:
            raise DevRunError("staged executable hash differs from the build")
        self.state["phase"] = "staged"
        self.save()
        verify_same_process(self.ops, self.state["old_process"])
        self.ops.rename("exe", "old", self.state["old_file"])
        self.state["phase"] = "parked"
        self.save()
        self.ops.rename("stage", "exe", self.state["new_file"])
        self.state["phase"] = "swapped"
        self.save()

    def rollback(self):
        # A timed-out bridge call may have completed. Inspect actual own files;
        # never treat an uncertain operation's Python flag as filesystem proof.
        old = self.ops.file("old")
        if old is not None:
            if old != self.state["old_file"]:
                raise DevRunError("rollback ownership changed; backup/lock retained")
            current = self.ops.file("exe")
            if current is not None:
                if current != self.state.get("new_file"):
                    raise DevRunError("rollback target changed; backup/lock retained")
                self.ops.rename("exe", "failed", current)
            self.ops.rename("old", "exe", old)
        elif self.ops.file("exe") != self.state["old_file"]:
            raise DevRunError("original image is unproved; lock retained")
        stage = self.ops.file("stage")
        if stage is not None:
            if stage != self.state.get("new_file"):
                raise DevRunError("stage ownership changed; lock retained")
            self.ops.remove("stage", stage)
        self.state["phase"] = "rolled-back"
        self.save()
        self.ops.unlock()

    def complete(self):
        self.state["phase"] = "complete"
        self.save()
        self.ops.write_receipt(self.state)
        self.ops.unlock()


def deploy(ops) -> dict:
    """One preflight/build/swap/restart; no restart or transaction retry."""
    status = ops.status(8)
    process = ops.process(status["pid"], 10)
    validate(status, process)
    baseline = process_key(process)
    image = windows_path(process["image"])
    transaction = Transaction(ops, process)
    transaction.begin()
    try:
        build = ops.build()
        verify_same_process(ops, baseline)
        transaction.swap(build)
        # Journal admission uncertainty before sending any POST.
        deadline = ops.clock() + 45
        ops.deadline = deadline
        transaction.state["phase"] = "posting"
        transaction.save()
        transaction.post_started = True
        try:
            ops.restart(min(8, deadline - ops.clock()))
        except Rejected:
            transaction.post_started = False
            ops.deadline = None
            raise
        transaction.state["phase"] = "admitted"
        transaction.save()
        while ops.clock() < deadline:
            remaining = deadline - ops.clock()
            try:
                observed = ops.status(min(8, remaining))
                remaining = deadline - ops.clock()
                if remaining <= 0:
                    break
                child = ops.process(observed["pid"], min(10, remaining))
                validate(observed, child, approved_image=image)
            except (AccessDenied, HTTPDeadline):
                raise
            except DevRunError:
                # The old process can lose its ports during handoff. Never
                # repost restart; the single overall deadline still applies.
                remaining = deadline - ops.clock()
                if remaining > 0:
                    ops.sleep(min(0.25, remaining))
                continue
            if ((child["pid"], child["created"]) != baseline[:2]
                    and ops.file("exe") == transaction.state["new_file"]
                    and observed["sha"] == build["sha"]):
                ops.accept_process(child)
                transaction.complete()
                return {"sha": observed["sha"], "channel": observed["channel"],
                        "cpu_pct_2s": observed.get("cpu_pct_2s"), "pid": child["pid"],
                        "created": child["created"], "exe_sha256": build["sha256"],
                        "sha_changed": observed["sha"] != status["sha"],
                        "qualification": "same-HEAD rebuild; new process/file proof, commit SHA unchanged"
                        if observed["sha"] == status["sha"] else "new process and commit SHA observed"}
            remaining = deadline - ops.clock()
            if remaining > 0:
                ops.sleep(min(0.25, remaining))
        raise DevRunError("restart outcome uncertain after 45s; backup/journal/lock retained")
    except Exception as error:
        if isinstance(error, FileOutcomeUnknown):
            raise DevRunError("Windows file operation lifetime/outcome uncertain; journal/lock retained, no rollback race") from None
        if transaction.post_started:
            raise DevRunError("restart admission/outcome uncertain; backup/journal/lock retained, no retry") from None
        try:
            ops.deadline = ops.clock() + 30
            transaction.rollback()
        except Exception:
            raise DevRunError("rollback could not prove/restore original image; backup/journal/lock retained") from None
        if isinstance(error, DevRunError):
            raise
        raise DevRunError("build or file operation failed; original pathname restored") from None


# Fixed local Windows bridge. Parameters are JSON stdin, never PowerShell
# interpolation. It has no HTTP, credentials, elevation, taskkill or relaunch.
WINDOWS_BRIDGE = r'''
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object Text.UTF8Encoding($false)
try {
  $q = [Console]::In.ReadToEnd() | ConvertFrom-Json
  Add-Type -TypeDefinition @'
using System; using System.Text; using System.Runtime.InteropServices;
public static class DevHudIdentity {
 [StructLayout(LayoutKind.Sequential)] public struct FT { public uint Low, High; }
 [StructLayout(LayoutKind.Sequential)] public struct FI {
  public uint Attributes; public FT Creation, Access, Write; public uint Volume, SizeHigh, SizeLow, Links, IndexHigh, IndexLow;
 }
 [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)] static extern IntPtr CreateFileW(string p,uint a,uint s,IntPtr sec,uint c,uint f,IntPtr t);
 [DllImport("kernel32.dll", SetLastError=true)] static extern bool GetFileInformationByHandle(IntPtr h,out FI i);
 [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)] static extern uint GetFinalPathNameByHandleW(IntPtr h,StringBuilder p,uint n,uint f);
 [DllImport("kernel32.dll", SetLastError=true)] static extern IntPtr OpenProcess(uint a,bool b,uint p);
 [DllImport("kernel32.dll", SetLastError=true)] static extern bool GetProcessTimes(IntPtr h,out FT c,out FT e,out FT k,out FT u);
 [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)] static extern bool QueryFullProcessImageNameW(IntPtr h,uint f,StringBuilder p,ref uint n);
 [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);
 public static string[] File(string p) {
  IntPtr h=CreateFileW(p,0,7,IntPtr.Zero,3,0x02200000,IntPtr.Zero);
  if(h==new IntPtr(-1)) throw new Exception("file identity unavailable");
  try { FI i; var b=new StringBuilder(32768);
   if(!GetFileInformationByHandle(h,out i)||(i.Attributes&0x400)!=0||GetFinalPathNameByHandleW(h,b,32768,0)==0) throw new Exception("ambiguous file identity");
   return new[]{b.ToString(),i.Volume.ToString()+":"+i.IndexHigh.ToString()+":"+i.IndexLow.ToString()};
  } finally {CloseHandle(h);}
 }
 public static string[] Process(uint pid) {
  IntPtr h=OpenProcess(0x1000,false,pid); if(h==IntPtr.Zero) throw new Exception("process identity unavailable");
  try {FT c,e,k,u; uint n=32768; var b=new StringBuilder(32768);
   if(!GetProcessTimes(h,out c,out e,out k,out u)||!QueryFullProcessImageNameW(h,0,b,ref n)) throw new Exception("process identity unavailable");
   return new[]{b.ToString(),(((ulong)c.High<<32)|c.Low).ToString()};
  } finally {CloseHandle(h);}
 }
}
'@
  function Canonical([string]$path, [bool]$missing=$false) {
    if ($path -notmatch '^[A-Za-z]:\\' -or $path.Contains('"')) { throw 'unsupported path' }
    $full = [IO.Path]::GetFullPath($path)
    $cursor = $full; $suffix = @()
    while (-not (Test-Path -LiteralPath $cursor)) {
      if (-not $missing) { throw 'missing identity' }
      $suffix = @([IO.Path]::GetFileName($cursor)) + $suffix
      $cursor = [IO.Path]::GetDirectoryName($cursor)
      if (-not $cursor) { throw 'unknown root' }
    }
    $check = $cursor
    while ($check) {
      $null = [DevHudIdentity]::File($check)
      $parent = [IO.Path]::GetDirectoryName($check)
      if ($parent -eq $check) { break }; $check = $parent
    }
    $identity = [DevHudIdentity]::File($cursor)
    $result = $identity[0] -replace '^\\\\\?\\',''
    foreach ($part in $suffix) { $result = [IO.Path]::Combine($result,$part) }
    return @{ path=$result; id=$(if($suffix.Count -eq 0){$identity[1]}else{$null}) }
  }
  function FileInfo([string]$path) {
    if (-not (Test-Path -LiteralPath $path)) { return $null }
    $i = Canonical $path
    return @{ id=$i.id; sha256=(Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() }
  }
  if ($q.op -eq 'inspect') {
    $raw = [DevHudIdentity]::Process([uint32]$q.pid)
    $image = Canonical $raw[0]; $dir = Canonical ([IO.Path]::GetDirectoryName($image.path))
    $dev = Canonical $q.dev_dir
    $prod = Canonical ([IO.Path]::Combine($env:LOCALAPPDATA,'Programs','tze_hud','tze_hud.exe')) $true
    $local = @((Get-NetIPAddress).IPAddress) + @('127.0.0.1','::1')
    $addresses = @([Net.Dns]::GetHostAddresses($q.host) | ForEach-Object {$_.ToString()})
    if ($addresses.Count -eq 0 -or @($addresses | Where-Object {$_ -notin $local}).Count -ne 0) {throw 'endpoint not local'}
    $listeners = @(Get-NetTCPConnection -State Listen -LocalPort ([int]$q.port) | Where-Object {
      $_.OwningProcess -eq [uint32]$q.pid -and ($_.LocalAddress -in @('0.0.0.0','::') -or $_.LocalAddress -in $addresses)
    })
    if ($listeners.Count -eq 0) {throw 'listener identity unproved'}
    $answer = @{pid=[int]$q.pid;created=$raw[1];image=$image.path;file_id=$image.id;directory_id=$dir.id;
                dev_dir=$dev.path;production=$prod.path;production_file_id=$prod.id;
                canonical_verified=$true;local_listener_verified=$true}
  } else {
    $dir = Canonical $q.directory
    if ($dir.id -ne $q.directory_id) {throw 'directory identity changed'}
    if ($q.token -notmatch '^[0-9a-f]{32}$') {throw 'invalid ownership token'}
    $paths = @{exe=[IO.Path]::Combine($dir.path,'tze_hud.exe');old=[IO.Path]::Combine($dir.path,'tze_hud.old.exe');
               stage=[IO.Path]::Combine($dir.path,('.tze_hud.'+$q.token+'.stage.exe'));
               failed=[IO.Path]::Combine($dir.path,('.tze_hud.'+$q.token+'.failed.exe'));
               lock=[IO.Path]::Combine($dir.path,'.tze_hud-dev-run.lock');receipt=[IO.Path]::Combine($dir.path,'.tze_hud-dev-run.json')}
    if ($q.op -notin @('file','receipt')) {
      $live=[DevHudIdentity]::Process([uint32]$q.guard.pid)
      if ($live[1] -ne $q.guard.created) {throw 'process creation identity changed'}
    }
    if ($q.op -eq 'lock') {
      $f=[IO.File]::Open($paths.lock,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::None)
      try {$b=[Text.Encoding]::UTF8.GetBytes((@{token=$q.token}|ConvertTo-Json -Compress));$f.Write($b,0,$b.Length);$f.Flush($true)}finally{$f.Dispose()}
      $answer = @{locked=$true}
    } elseif ($q.op -in @('file','receipt')) {
      if ($q.op -eq 'file') {
        if ($q.label -notin @('exe','old','stage','failed')) {throw 'unknown slot'}
        $answer = FileInfo $paths[$q.label]
      } elseif (Test-Path -LiteralPath $paths.receipt) {
        $null=Canonical $paths.receipt
        if ((Get-Item -LiteralPath $paths.receipt).Length -gt 8192) {throw 'oversized receipt'}
        $answer = [IO.File]::ReadAllText($paths.receipt)|ConvertFrom-Json
      } else {$answer=$null}
    } else {
      $null=Canonical $paths.lock
      if ((Get-Item -LiteralPath $paths.lock).Length -gt 8192) {throw 'oversized lock'}
      $owner=[IO.File]::ReadAllText($paths.lock)|ConvertFrom-Json
      if ($owner.token -ne $q.token) {throw 'not own transaction'}
      switch ($q.op) {
        'copy' {
          $f=[IO.File]::Open($paths.stage,[IO.FileMode]::CreateNew,[IO.FileAccess]::Write,[IO.FileShare]::None)
          try {$src=[IO.File]::OpenRead($q.source);try{$src.CopyTo($f);$f.Flush($true)}finally{$src.Dispose()}}finally{$f.Dispose()}
          $answer=FileInfo $paths.stage
        }
        'rename' {
          if (($q.source_label+':'+$q.dest_label) -notin @('exe:old','stage:exe','exe:failed','old:exe')) {throw 'unknown rename'}
          $info=FileInfo $paths[$q.source_label]
          if ($info.id -ne $q.expected.id -or $info.sha256 -ne $q.expected.sha256) {throw 'source identity changed'}
          [IO.File]::Move($paths[$q.source_label],$paths[$q.dest_label]);$answer=@{renamed=$true}
        }
        'remove' {
          if ($q.label -notin @('old','stage')) {throw 'unknown removal'}
          $info=FileInfo $paths[$q.label]
          if ($info.id -ne $q.expected.id -or $info.sha256 -ne $q.expected.sha256) {throw 'file identity changed'}
          [IO.File]::Delete($paths[$q.label]);$answer=@{removed=$true}
        }
        'journal' {[IO.File]::WriteAllText($paths.lock,($q.state|ConvertTo-Json -Depth 8 -Compress));$answer=@{saved=$true}}
        'receipt-write' {
          if (Test-Path -LiteralPath $paths.receipt) {$null=Canonical $paths.receipt}
          [IO.File]::WriteAllText($paths.receipt,($q.state|ConvertTo-Json -Depth 8 -Compress));$answer=@{saved=$true}
        }
        'unlock' {[IO.File]::Delete($paths.lock);$answer=@{unlocked=$true}}
        default {throw 'unknown operation'}
      }
    }
  }
  ConvertTo-Json -InputObject $answer -Depth 8 -Compress
} catch { [Console]::Error.WriteLine('dev-run Windows identity/file operation failed'); exit 1 }
'''


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


class NativeOps:
    def __init__(self, repo: Path, dev_dir: str, host: str | None):
        # Refuse unsupported agent hosts before credential loading or HTTP.
        self.powershell = shutil.which("powershell.exe")
        self.wslpath = shutil.which("wslpath")
        if not self.powershell or not self.wslpath:
            raise DevRunError("dev-run requires local WSL Windows interop (powershell.exe and wslpath)")
        self.repo = repo
        self.dev_dir = dev_dir
        self.token = uuid.uuid4().hex
        self.directory = None
        self.directory_id = None
        self.guard_process = None
        self.clock = time.monotonic
        self.sleep = time.sleep
        self.deadline = None
        helper = repo / ".claude/skills/user-test/scripts/hud_env.py"
        spec = importlib.util.spec_from_file_location("dev_run_hud_env", helper)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        if host:
            # Existing strict explicit resolver, without persistent HUD_HOST.
            selected_host, url = module._adapter_url(host)
            endpoint = {"host": selected_host, "url": url, "explicit": True}
        else:
            endpoint = module.adapter_endpoint()
        self.key = module.adapter_key(endpoint)
        self.origin = endpoint["url"].removesuffix("/mcp")
        parsed = urllib.parse.urlsplit(self.origin)
        self.host, self.port = parsed.hostname, parsed.port
        self.http = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
        self.encoded = base64.b64encode(WINDOWS_BRIDGE.encode("utf-16le")).decode()

    def request(self, method, route, timeout):
        if self.deadline is not None:
            timeout = min(timeout, self.deadline - self.clock())
        if timeout <= 0:
            raise DevRunError("request deadline expired")
        request = urllib.request.Request(self.origin + route, method=method,
                                         data=b"" if method == "POST" else None,
                                         headers={"Authorization": "Bearer " + self.key})
        try:
            # Socket timeout alone is per-I/O: a drip-fed body could otherwise
            # outlive the overall request/observation budget.
            with request_wall_deadline(timeout):
                with self.http.open(request, timeout=timeout) as response:
                    data = response.read(65537)
                    if len(data) > 65536:
                        raise DevRunError("oversized HUD response")
                    result = json.loads(data)
                    if not isinstance(result, dict):
                        raise DevRunError("malformed HUD response")
                    return response.status, result
        except urllib.error.HTTPError as error:
            if method == "POST" and error.code in (401, 403, 429, 503):
                raise Rejected(f"restart rejected: HTTP {error.code}") from None
            if error.code in (401, 403):
                raise AccessDenied(f"HUD access denied: HTTP {error.code}") from None
            raise DevRunError(f"HUD request rejected: HTTP {error.code}") from None
        except (OSError, ValueError):
            raise DevRunError("HUD request/response unavailable or malformed") from None

    def status(self, timeout):
        code, status = self.request("GET", "/admin/status", timeout)
        if code != 200:
            raise DevRunError("unexpected HUD status response")
        return status

    def restart(self, timeout):
        code, response = self.request("POST", "/admin/restart", timeout)
        if code != 202 or response != {"restarting": True}:
            raise DevRunError("restart acceptance unproved")

    def bridge(self, op, timeout=10, **values):
        if self.deadline is not None:
            timeout = min(timeout, self.deadline - self.clock())
        if timeout <= 0:
            raise DevRunError("Windows observation deadline expired")
        request = {"op": op, "token": self.token, "directory": self.directory,
                   "directory_id": self.directory_id, "guard": self.guard_process, **values}
        try:
            result = subprocess.run([self.powershell, "-NoLogo", "-NoProfile", "-NonInteractive",
                                     "-EncodedCommand", self.encoded],
                                    input=json.dumps(request).encode(), capture_output=True,
                                    timeout=timeout, check=False)
            if result.returncode or len(result.stdout) > 16384:
                raise DevRunError("Windows identity/file operation failed")
            return json.loads(result.stdout.decode("utf-8-sig"))
        except subprocess.TimeoutExpired:
            if op not in ("inspect", "file", "receipt"):
                raise FileOutcomeUnknown("Windows writer lifetime/outcome unproved") from None
            raise DevRunError("Windows observation deadline expired") from None
        except (OSError, ValueError):
            raise DevRunError("Windows identity/file operation outcome unproved") from None

    def process(self, pid, timeout):
        result = self.bridge("inspect", timeout, pid=pid, host=self.host, port=self.port, dev_dir=self.dev_dir)
        directory = ntpath.dirname(result["image"])
        if self.directory is not None:
            if (windows_path(directory) != windows_path(self.directory)
                    or result["directory_id"] != self.directory_id):
                raise DevRunError("native target directory identity changed")
        else:
            self.directory = directory
            self.directory_id = result["directory_id"]
            self.guard_process = {"pid": result["pid"], "created": result["created"]}
        return result

    def accept_process(self, child):
        self.guard_process = {"pid": child["pid"], "created": child["created"]}

    def build(self):
        try:
            subprocess.run(["just", "build-windows"], cwd=self.repo, check=True, timeout=600)
            result = subprocess.run(["cargo", "metadata", "--no-deps", "--format-version", "1"],
                                    cwd=self.repo, capture_output=True, check=True, timeout=30)
            target = Path(json.loads(result.stdout)["target_directory"])
            image = target / "x86_64-pc-windows-gnu/release/tze_hud.exe"
            with image.open("rb") as stream:
                header = stream.read(64)
                if len(header) != 64 or header[:2] != b"MZ":
                    raise DevRunError("build output is not a Windows executable")
                offset = struct.unpack_from("<I", header, 60)[0]
                if not 64 <= offset <= 1048576:
                    raise DevRunError("invalid PE header offset")
                stream.seek(offset)
                pe = stream.read(24)
                if len(pe) != 24 or pe[:6] != b"PE\0\0\x64\x86":
                    raise DevRunError("build output is not an x86_64 PE executable")
                characteristics = struct.unpack_from("<H", pe, 22)[0]
                if not characteristics & 2 or characteristics & 0x2000:
                    raise DevRunError("build output is not a PE application")
            digest = hashlib.sha256(image.read_bytes()).hexdigest()
            converted = subprocess.run([self.wslpath, "-w", str(image)], capture_output=True,
                                       check=True, timeout=10).stdout.decode().strip()
            commit = subprocess.run(["git", "rev-parse", "HEAD"], cwd=self.repo,
                                    capture_output=True, check=True, timeout=10).stdout.decode().strip()
            return {"path": converted, "sha256": digest, "sha": commit}
        except (OSError, ValueError, KeyError, subprocess.SubprocessError):
            raise DevRunError("Windows build/output verification failed") from None

    def lock(self):
        self.bridge("lock")

    def unlock(self):
        self.bridge("unlock")

    def file(self, label):
        return self.bridge("file", label=label)

    def receipt(self):
        return self.bridge("receipt")

    def journal(self, state):
        self.bridge("journal", state=state)

    def write_receipt(self, state):
        self.bridge("receipt-write", state=state)

    def copy_stage(self, build):
        return self.bridge("copy", timeout=60, source=build["path"])

    def rename(self, source, destination, expected):
        self.bridge("rename", source_label=source, dest_label=destination, expected=expected)

    def remove(self, label, expected):
        self.bridge("remove", label=label, expected=expected)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dev-dir", required=True, help="existing intended Windows dev directory")
    parser.add_argument("--host", default="", help="paired local Windows HUD host[:MCP-port]")
    args = parser.parse_args(argv)
    try:
        result = deploy(NativeOps(Path(__file__).resolve().parents[1], args.dev_dir, args.host or None))
        print(json.dumps(result, sort_keys=True))
        return 0
    except DevRunError as error:
        print(f"dev-run: {error}", file=sys.stderr)
        return 1
    except Exception:
        print("dev-run: unexpected local failure; inspect transaction state before retrying", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Export a rooted dada's code and firmware for skyward-pack --hardware.

Only immutable hardware files are exported. No data, metadata, persist,
credentials, or device identifiers are included. Python 3.11+ is required.
"""
import argparse
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import re
import shlex
import struct
import subprocess
import tarfile
import tomllib


def elf_needed(data):
    """Read DT_NEEDED from an ELF64 little-endian file without executing it."""
    if data[:4] != b"\x7fELF":
        return []
    if data[4:6] != b"\x02\x01":
        raise ValueError("expected a 64-bit little-endian hardware ELF")
    phoff = struct.unpack_from("<Q", data, 32)[0]
    size, count = struct.unpack_from("<HH", data, 54)
    headers = [struct.unpack_from("<IIQQQQQQ", data, phoff + k * size) for k in range(count)]
    dynamic = next((h for h in headers if h[0] == 2), None)
    if dynamic is None:
        return []
    tags = [struct.unpack_from("<qQ", data, p) for p in range(dynamic[2], dynamic[2] + dynamic[5], 16)]
    address = next(v for t, v in tags if t == 5)
    load = next(h for h in headers if h[0] == 1 and h[3] <= address < h[3] + h[5])
    strings = load[2] + address - load[3]
    result = []
    for tag, offset in tags:
        if tag == 1:
            start = strings + offset
            result.append(data[start:data.index(b"\0", start)].decode("ascii"))
    return result


def safe_path(path):
    value = PurePosixPath(path)
    if value.is_absolute() or any(p in (".", "..", "") for p in path.split("/")) or any(c in path for c in "\\\r\n\0:"):
        raise ValueError(f"unsafe archive path: {path}")
    if not value.parts or value.parts[0] not in ("vendor", "odm", "system", "apex", "vendor_dlkm", "system_dlkm"):
        raise ValueError(f"not an immutable hardware path: {path}")
    return value


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--adb", default="adb")
    parser.add_argument("--serial")
    parser.add_argument("--config", type=Path, default=Path("devices/dada.toml"))
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    cfg = tomllib.loads(args.config.read_text(encoding="utf-8"))
    if cfg["device"]["codename"] != "dada":
        parser.error("this firmware selection is currently defined only for dada")
    if args.out.exists():
        parser.error("--out must be a new directory")
    adb = [args.adb] + (["-s", args.serial] if args.serial else [])

    def command(words, **kwargs):
        result = subprocess.run(adb + words, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=180, **kwargs)
        if result.returncode:
            raise RuntimeError(result.stderr.decode("utf-8", errors="replace"))
        return result.stdout

    def shell(script):
        return command(["exec-out", "su", "-c", script]).decode("utf-8")

    if "uid=0" not in shell("id"):
        raise RuntimeError("a rooted Android session is required")
    properties = {}
    for line in shell("getprop").splitlines():
        match = re.fullmatch(r"\[([^]]+)\]: \[(.*)\]", line)
        if match:
            key, value = match.groups()
            if key.startswith(("ro.build.", "ro.vendor.", "ro.product.", "ro.crypto.", "ro.soc.")) or key in (
                "ro.hardware", "ro.mi.os.version.incremental", "ro.vndk.version",
            ):
                if not any(p in key.lower() for p in ("serial", "imei", "macaddr")):
                    properties[key] = value
    device = shell("getprop ro.product.device").strip()
    if device != "dada":
        raise RuntimeError(f"connected device is {device}, expected dada")
    release = shell("uname -r").strip()
    slot = shell("getprop ro.boot.slot_suffix").strip()
    if slot not in ("_a", "_b"):
        raise RuntimeError(f"invalid current slot: {slot!r}")
    roots = ["/vendor/bin", "/odm/bin", "/vendor/lib64", "/odm/lib64", "/system/lib64", "/apex", "/vendor/etc", "/odm/etc",
             "/vendor/firmware", "/vendor/firmware_mnt/image", "/vendor_dlkm/lib/modules", "/system_dlkm/lib/modules"]
    index = set(shell("find " + " ".join(roots) + " -type f").splitlines())
    index = {p for p in index if not re.match(r"/apex/[^/]*@", p)}
    by_name = {}
    for path in sorted(index):
        by_name.setdefault(PurePosixPath(path).name, []).append(path)
    files = {}
    args.out.mkdir(parents=True)
    local_list = args.out / ".files"
    remote_list = "/data/local/tmp/skyward-hardware-files.txt"

    def take(paths):
        paths = sorted(set(paths) - files.keys())
        if not paths:
            return
        for path in paths:
            safe_path(path.lstrip("/"))
            if "\n" in path or "\r" in path:
                raise ValueError("a hardware path contains a newline")
        local_list.write_text("".join(p.lstrip("/") + "\n" for p in paths), encoding="utf-8", newline="\n")
        command(["push", str(local_list), remote_list])
        # Refuse symlinks into mutable storage before tar dereferences any
        # input. Vendor firmware contains links into data and persist too.
        shell(f'''while IFS= read -r immutable_path; do
            test -f "/$immutable_path" || exit 1
            immutable_target=$(readlink -f "/$immutable_path") || exit 1
            case "$immutable_target" in
                /vendor/*|/odm/*|/system/*|/apex/*|/vendor_dlkm/*|/system_dlkm/*) ;;
                *) printf 'not immutable: %s\\n' "$immutable_path"; exit 1 ;;
            esac
        done < {remote_list}''')
        archive = command(["exec-out", "su", "-c", f"tar -chf - -C / -T {remote_list} 2>/data/local/tmp/skyward-hardware-tar-errors.txt"])
        errors = shell("cat /data/local/tmp/skyward-hardware-tar-errors.txt")
        if errors.strip():
            print(errors, flush=True)
        with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
            for member in tar:
                relative = safe_path(member.name)
                if not (member.isfile() or member.islnk()):
                    raise ValueError(f"unexpected non-file: {member.name}")
                content = tar.extractfile(member).read()
                path = "/" + str(relative)
                target = args.out.joinpath(*relative.parts)
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(content)
                files[path] = (member.mode & 0o777, content)
        if any(p not in files for p in paths):
            raise RuntimeError("the archive omitted a requested hardware file")
        print(f"exported {len(files)} files, {sum(len(v[1]) for v in files.values()) // 1024} KiB", flush=True)

    try:
        selected = {s["path"] for s in cfg.get("service", []) if s["path"].startswith(("/vendor/", "/odm/"))}
        selected.add(cfg["wlan"]["supplicant"])
        selected.update(a for s in cfg.get("service", []) for a in s.get("args", []) if a.startswith(("/vendor/", "/odm/")))
        selected.update(s["description"] for s in cfg.get("service", []) if s.get("description", "").startswith(("/vendor/", "/odm/")))
        selected.add("/apex/com.android.runtime/bin/linker64")
        selected.update(p for p in index if p.startswith("/apex/com.android.runtime/lib64/bionic/") and "/hwasan/" not in p)
        selected.update(p for p in index if p.startswith("/vendor/bin/init.kernel.post_boot-") and p.endswith(".sh"))
        selected.update(p for p in index if p.startswith(("/vendor/etc/vintf/", "/odm/etc/vintf/", "/vendor/etc/ssg/")))
        selected.update(p for p in index if PurePosixPath(p).name in ("passwd", "group", "ueventd.rc", "gpfspath_oem_config.xml"))
        selected.update(p for p in index if re.search(r"/(libnfc[^/]*\.conf|gptauuid\.xml)$", p))
        selected.update(f["path"] for f in cfg["wlan"].get("firmware", []))
        selected.update(f["path"] for f in cfg.get("haptics", {}).get("firmware", []))
        selected.update(cfg["haptics"][key]["path"] for key in ("click_rtp", "ticks_rtp")
                        if cfg.get("haptics", {}).get(key))
        # Keep ADSP firmware for the stock recovery's PMIC/charging startup,
        # plus secure applications and Peach firmware.
        excluded = re.compile(r"^(fingerpr|gf|cdsp|modem|mba|oemvm|slpi|venus|vpu|camera)")
        peach_v1 = {"peach/amss.bin", "peach/Data.msc", "peach/phy_ucode.elf", "peach/aux_ucode.elf", "tmel_peach_10.elf"}
        use_peach_v2 = "qca_cld3_peach_v2" in cfg["wlan"]["modules"]
        selected.update(p for p in index if p.startswith("/vendor/firmware_mnt/image/") and
                        not excluded.match(p.removeprefix("/vendor/firmware_mnt/image/")) and
                        not (use_peach_v2 and p.removeprefix("/vendor/firmware_mnt/image/") in peach_v1))
        selected.update(p for p in index if p.startswith("/vendor/firmware/") and "/wlan/" not in p and not excluded.match(PurePosixPath(p).name))
        programs = [p for p in selected if p.startswith(("/vendor/bin/", "/odm/bin/"))]
        cases = "|".join(shlex.quote(p) for p in programs)
        mapped = shell(f'''for hardware_pid in /proc/[0-9]*; do
            hardware_exe=$(readlink "$hardware_pid/exe" 2>/dev/null) || continue
            case "$hardware_exe" in {cases})
                awk '$6 ~ /^\\/(vendor|odm|system|apex)\\// && $6 ~ /\\/lib64\\// {{print $6}}' "$hardware_pid/maps" ;;
            esac
        done''')
        selected.update(p for p in mapped.splitlines() if p in index)
        selected.update(p for name in ("ese_weaver_thales.so", "libGPMTEEC_vendor.so", "libGPTEE_vendor.so", "libtaautoload.so", "libminksocket_vendor.so", "libGPQeSE.so")
                        for p in by_name.get(name, []) if "/lib64/" in p)
        take(selected)

        # Only explicit configuration paths, not arbitrary strings or ELF
        # library names. This covers scripts, QMI config and Wi-Fi overlays
        # without pulling framework or another platform's libraries.
        checked_config = set()

        def take_config():
            while True:
                wanted = set()
                for path, (_, data) in list(files.items()):
                    if path in checked_config:
                        continue
                    checked_config.add(path)
                    if not (path.startswith(("/vendor/bin/", "/odm/bin/")) or "/lib64/" in path):
                        continue
                    refs = re.findall(rb"/(?:vendor|odm|apex)/[a-zA-Z0-9_.\-/]+", data)
                    wanted.update(p for raw in refs if "/etc/" in (p := raw.decode("ascii")) and p in index)
                if not (wanted - files.keys()):
                    break
                take(wanted)

        take_config()

        # Static dependencies come from DT_NEEDED. Late-loaded libraries
        # come from the running hardware services' maps and known eSE plug-ins.
        checked = set()
        while True:
            wanted = set()
            for path, (_, data) in list(files.items()):
                if path in checked or data[:4] != b"\x7fELF":
                    continue
                checked.add(path)
                if not ("/lib64/" in path or path.startswith(("/vendor/bin/", "/odm/bin/", "/apex/"))):
                    continue  # Secure applications are not Android programs.
                needed = set(elf_needed(data))
                for name in needed:
                    candidates = by_name.get(name, [])
                    candidates = [p for p in candidates if "/lib64/" in p]
                    if not candidates:
                        raise RuntimeError(f"{path} needs {name}, which is not in any exported library source")
                    if any(p in files for p in candidates):
                        continue
                    priority = lambda p: next((k for k, prefix in enumerate((
                        '/system/lib64/', '/vendor/lib64/', '/odm/lib64/', '/apex/com.android.runtime/', '/apex/',
                    )) if p.startswith(prefix)), 100)
                    wanted.add(min(candidates, key=lambda p: (priority(p), p)))
            if not (wanted - files.keys()):
                break
            take(wanted)

        take_config()
        used_apex = {p.split('/')[2] for p in files if p.startswith('/apex/')}
        take(p for p in index if any(p.startswith(f'/apex/{name}/etc/') for name in used_apex))

        graph = {}
        dep = shell("cat /vendor_dlkm/lib/modules/modules.dep")
        for line in dep.splitlines():
            left, _, right = line.partition(":")
            graph[PurePosixPath(left).name] = [PurePosixPath(p).name for p in right.split()]
        required = [name + ".ko" for name in cfg["modules"]["load"] + cfg["wlan"]["modules"] + cfg.get("haptics", {}).get("modules", [])]
        modules = set()

        def visit(name):
            if name in modules:
                return
            modules.add(name)
            for child in graph.get(name, []):
                visit(child)

        for name in required:
            visit(name)
        paths = {}
        for name in modules:
            candidates = [p for p in by_name.get(name, []) if "/lib/modules/" in p]
            if not candidates:
                raise RuntimeError(f"missing kernel module: {name}")
            paths[name] = candidates[0]
        take(paths.values())
        module_dir = args.out / "modules"
        module_dir.mkdir()
        for name, path in paths.items():
            (module_dir / name).write_bytes(files[path][1])
        (module_dir / "modules.dep").write_text("".join(f"{n}: {' '.join(graph.get(n, []))}\n" for n in sorted(modules)), encoding="utf-8", newline="\n")
        (args.out / "build.prop").write_text("".join(f"{k}={v}\n" for k, v in sorted(properties.items())), encoding="utf-8", newline="\n")
        boot = command(["exec-out", "su", "-c", f"dd if=/dev/block/by-name/boot{slot} bs=1048576"])
        if boot[:8] != b"ANDROID!" or struct.unpack_from("<I", boot, 40)[0] not in (3, 4):
            raise RuntimeError("the source boot image has an unsupported header")
        (args.out / "boot.img").write_bytes(boot)
        kernel_size = struct.unpack_from('<I', boot, 8)[0]
        kernel_digest = hashlib.sha256(boot[4096:4096 + kernel_size]).hexdigest()
        manifest = ["version = 1\n", f"device = {json.dumps(device)}\n", f"kernel_release = {json.dumps(release)}\n",
                    f"os_version = {json.dumps(properties.get('ro.mi.os.version.incremental', ''))}\n",
                    f"kernel_sha256 = {json.dumps(kernel_digest)}\n"]
        payloads = []
        for source in sorted(args.out.rglob("*")):
            if not source.is_file() or source.name in (".files", "boot.img"):
                continue
            relative = source.relative_to(args.out).as_posix()
            # Modules are flattened into their own source; do not pack the
            # original partition paths a second time.
            if relative.startswith(("vendor_dlkm/", "system_dlkm/")):
                continue
            mode = files.get("/" + relative, (0o644, b""))[0]
            digest = hashlib.sha256(source.read_bytes()).hexdigest()
            manifest.extend(["\n[[file]]\n", f"path = {json.dumps(relative)}\n", f"sha256 = {json.dumps(digest)}\n", f"mode = {mode}\n"])
            payloads.append(relative)
        (args.out / "manifest.toml").write_text("".join(manifest), encoding="utf-8", newline="\n")
        print(f"complete: {len(payloads)} payload files; {release}; {properties.get('ro.mi.os.version.incremental')}")
    finally:
        command(["shell", "rm", "-f", remote_list])
        command(["shell", "rm", "-f", "/data/local/tmp/skyward-hardware-tar-errors.txt"])
        local_list.unlink(missing_ok=True)


if __name__ == "__main__":
    main()

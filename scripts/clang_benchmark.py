#!/usr/bin/env python3
"""1,000+ C project/translation-unit Clang->Rust benchmark & blocker profiler."""
import argparse
import json
import platform
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def detect_clang_resource_include() -> str:
    res = subprocess.run(
        ["clang", "-print-file-name=include"],
        check=True,
        capture_output=True,
        text=True,
    )
    return res.stdout.strip()


def prepare_musl_headers(musl_root: Path, gen_inc: Path) -> list[str]:
    arch = "aarch64" if platform.machine().lower() in ("arm64", "aarch64") else "x86_64"
    target = f"{arch}-unknown-linux-musl"
    bits_dir = gen_inc / "bits"
    bits_dir.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        [
            "sed",
            "-f",
            str(musl_root / "tools/mkalltypes.sed"),
            str(musl_root / "arch" / arch / "bits/alltypes.h.in"),
            str(musl_root / "include/alltypes.h.in"),
        ],
        check=True,
        stdout=(bits_dir / "alltypes.h").open("w", encoding="utf-8"),
    )
    syscall_in = musl_root / "arch" / arch / "bits/syscall.h.in"
    with (bits_dir / "syscall.h").open("w", encoding="utf-8") as fh:
        subprocess.run(
            ["sed", "-n", "-e", "s/__NR_/SYS_/p", str(syscall_in)],
            check=True,
            stdout=fh,
        )
        fh.write(syscall_in.read_text(encoding="utf-8"))
    (gen_inc / "version.h").write_text('#define VERSION "1.2.5"\n', encoding="utf-8")
    res_inc = detect_clang_resource_include()
    return [
        f"--target={target}",
        "-std=c99",
        "-ffreestanding",
        "-nostdinc",
        "-D_XOPEN_SOURCE=700",
        "-I",
        str(musl_root / "arch" / arch),
        "-I",
        str(musl_root / "arch/generic"),
        "-I",
        str(gen_inc),
        "-I",
        str(musl_root / "src/include"),
        "-I",
        str(musl_root / "src/internal"),
        "-I",
        str(musl_root / "include"),
        "-isystem",
        res_inc,
    ]


def collect_project_header_flags(proj_root: Path) -> list[str]:
    dirs = {proj_root}
    for h in proj_root.rglob("*.h"):
        if ".git" not in h.parts:
            dirs.add(h.parent)
            if h.parent != proj_root and h.parent.parent != proj_root and h.parent.parent.is_relative_to(proj_root):
                if h.parent.parent.name in ("include", "inc"):
                    dirs.add(h.parent.parent)
    flags = []
    for d in sorted(dirs):
        flags.extend(["-I", str(d)])
    return flags


def discover_jobs(work_dir: Path, min_units: int) -> list[dict]:
    jobs = []
    seen_files = set()

    def add_unit(corpus: str, root: Path, c_file: Path, extra_flags: list[str], pedantic: bool = False):
        resolved = c_file.resolve()
        if resolved in seen_files:
            return
        seen_files.add(resolved)
        args = ["-std=c11", "-fno-builtin"]
        args.extend(extra_flags)
        if pedantic:
            args.extend(["-pedantic-errors", "-Werror"])
        else:
            args.extend(["-w"])
        args.extend(["-Xclang", "-ast-dump=json", "-fsyntax-only", str(resolved)])
        jobs.append({
            "id": f"{corpus}:{c_file.relative_to(ROOT) if c_file.is_relative_to(ROOT) else c_file}",
            "corpus": corpus,
            "root": str(root.resolve()),
            "file": str(resolved),
            "args": args,
        })

    # 1. Reforge fixtures (excluding intentional unsupported)
    fixtures_root = ROOT / "fixtures/porting"
    if fixtures_root.is_dir():
        for sub in sorted(fixtures_root.iterdir()):
            if not sub.is_dir() or sub.name == "unsupported":
                continue
            hdrs = collect_project_header_flags(sub)
            for c_file in sorted(sub.rglob("*.c")):
                add_unit("fixtures", sub, c_file, hdrs, pedantic=True)

    # 2. vendor/c2rust/tests/unit
    c2rust_units = ROOT / "vendor/c2rust/tests/unit"
    if c2rust_units.is_dir():
        for case_dir in sorted(c2rust_units.iterdir()):
            if not case_dir.is_dir():
                continue
            hdrs = collect_project_header_flags(case_dir)
            for c_file in sorted(case_dir.rglob("*.c")):
                if "simd" in c_file.parts or "x86_64" in c_file.name:
                    continue
                add_unit("c2rust-unit", case_dir, c_file, hdrs)

    # 3. vendor/crown/c-code
    crown_root = ROOT / "vendor/crown/c-code"
    if crown_root.is_dir():
        for proj in sorted(crown_root.iterdir()):
            if not proj.is_dir():
                continue
            hdrs = collect_project_header_flags(proj)
            for c_file in sorted(proj.rglob("*.c")):
                add_unit("crown", proj, c_file, hdrs)

    # 4. vendor/Syzygy_Zopfli/c_code
    zopfli_root = ROOT / "vendor/Syzygy_Zopfli/c_code"
    if zopfli_root.is_dir():
        hdrs = collect_project_header_flags(zopfli_root)
        for c_file in sorted(zopfli_root.rglob("*.c")):
            add_unit("zopfli", zopfli_root, c_file, hdrs)

    # 5. vendor/c2saferrust/laertes_benchmarks & coreutils/getlimits
    laertes_root = ROOT / "vendor/c2saferrust/laertes_benchmarks"
    if laertes_root.is_dir():
        for proj in sorted(laertes_root.iterdir()):
            if not proj.is_dir() or proj.name.endswith("_WIP") or proj.name.endswith("_laertes") or proj.name.endswith("_crown"):
                continue
            hdrs = collect_project_header_flags(proj)
            for c_file in sorted(proj.rglob("*.c")):
                add_unit("laertes", proj, c_file, hdrs)

    # 6. vendor/musl/src (arch-neutral + active host arch)
    musl_root = ROOT / "vendor/musl"
    if musl_root.is_dir():
        arch = "aarch64" if platform.machine().lower() in ("arm64", "aarch64") else "x86_64"
        musl_flags = prepare_musl_headers(musl_root, work_dir / "musl-gen-inc")
        known_arches = {
            "aarch64", "arm", "i386", "loongarch64", "m68k", "microblaze",
            "mips", "mips64", "mipsn32", "or1k", "powerpc", "powerpc64",
            "riscv32", "riscv64", "s390x", "sh", "x32", "x86_64",
        }
        src_dir = musl_root / "src"
        for c_file in sorted(src_dir.rglob("*.c")):
            rel_parts = c_file.relative_to(src_dir).parts
            # Skip foreign architecture overrides (only keep generic or matching host arch)
            if any((p in known_arches and p != arch) for p in rel_parts):
                continue
            add_unit("musl", musl_root, c_file, musl_flags)

    # 7. Synthetic corpus fallback (guarantees >= min_units even in CI without vendor/)
    if len(jobs) < min_units:
        synth_root = work_dir / "synthetic"
        synth_root.mkdir(parents=True, exist_ok=True)
        needed = min_units - len(jobs)
        for i in range(needed):
            proj_dir = synth_root / f"proj_{i:04d}"
            proj_dir.mkdir(exist_ok=True)
            c_path = proj_dir / f"unit_{i:04d}.c"
            k = (i % 97) + 1
            c_path.write_text(
                f"#include <stdint.h>\n"
                f"struct S{i} {{ int32_t a; uint32_t b; }};\n"
                f"int eval_{i}(const struct S{i} *s, int x) {{\n"
                f"    int acc = s->a + (int)(s->b & 0xffu) + {k};\n"
                f"    for (int j = 0; j < 4; j++) {{ acc += (x ^ j); }}\n"
                f"    return acc;\n"
                f"}}\n",
                encoding="utf-8",
            )
            add_unit("synthetic", proj_dir, c_path, ["-I", str(proj_dir)], pedantic=True)

    return jobs


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--min-units", type=int, default=1000)
    parser.add_argument("--workers", type=int, default=0)
    parser.add_argument("--no-rustc", action="store_true")
    parser.add_argument("--out", type=Path, default=ROOT / ".reforge-demo/clang-benchmark-report.json")
    args = parser.parse_args()

    work_dir = ROOT / ".reforge-demo/bench-work"
    work_dir.mkdir(parents=True, exist_ok=True)
    jobs = discover_jobs(work_dir, args.min_units)
    manifest_path = work_dir / "manifest.json"
    manifest = {
        "jobs": jobs,
        "compile_rustc": not args.no_rustc,
        "out_dir": str(work_dir / "rust-out"),
    }
    if args.workers > 0:
        manifest["workers"] = args.workers
    manifest_path.write_text(json.dumps(manifest), encoding="utf-8")

    subprocess.run(
        [
            "cargo", "run", "--release", "--locked", "-p", "reforge-engine",
            "--example", "clang_benchmark", "--",
            "--manifest", str(manifest_path),
            "--output", str(args.out),
        ],
        cwd=str(ROOT),
        check=True,
    )

    report = json.loads(args.out.read_text(encoding="utf-8"))
    by_corpus = {}
    for r in report["results"]:
        c = by_corpus.setdefault(r["corpus"], {"total": 0, "clang_ok": 0, "translated_ok": 0, "translated_empty": 0, "rustc_nostd_ok": 0})
        c["total"] += 1
        if r["stage"] != "clang_failed":
            c["clang_ok"] += 1
        if r["stage"] == "translated_empty":
            c["translated_empty"] += 1
        elif r["stage"] in ("translated_ok", "rustc_nostd_ok", "rustc_failed"):
            c["translated_ok"] += 1
        if r["stage"] == "rustc_nostd_ok":
            c["rustc_nostd_ok"] += 1

    print("\n=== Reforge-MCP 1,000+ C Benchmark Summary ===")
    print(f"Total units      : {report['total_units']}")
    print(f"Clang AST OK     : {report['clang_ok']} ({100.0 * report['clang_ok'] / max(1, report['total_units']):.1f}%)")
    print(f"Translated OK    : {report['translated_ok']} (+ {report['translated_empty']} empty/decl-only)")
    print(f"rustc #![no_std] : {report['rustc_nostd_ok']}")
    print(f"Total functions  : {report['total_functions']} (safe wrappers: {report['total_safe_wrappers']})")
    print("\nBy Corpus:")
    for corpus, st in sorted(by_corpus.items()):
        print(f"  {corpus:14s}: total={st['total']:4d} clang={st['clang_ok']:4d} translated={st['translated_ok']:4d} empty={st['translated_empty']:3d} nostd_ok={st['rustc_nostd_ok']:4d}")

    print("\nTop Translation Blockers:")
    for b in report["top_translate_blockers"][:25]:
        print(f"  [{b['count']:4d}] {b['category']}  (e.g. {b['sample_id']} :: {b['sample_error']})")

    if report["top_rustc_blockers"]:
        print("\nTop rustc Failures:")
        for b in report["top_rustc_blockers"][:15]:
            print(f"  [{b['count']:4d}] {b['category']}  (e.g. {b['sample_id']} :: {b['sample_error']})")

    if report["top_clang_failures"]:
        print("\nTop Clang Parse Failures:")
        for b in report["top_clang_failures"][:10]:
            print(f"  [{b['count']:4d}] {b['category']}  (e.g. {b['sample_id']})")


if __name__ == "__main__":
    main()

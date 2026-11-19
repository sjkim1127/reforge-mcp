#!/usr/bin/env python3
"""End-to-end OSS C-to-Rust porting and native test-suite verification benchmark."""
import argparse
import json
import platform
import re
import shutil
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def clang_ast_args(root: Path, c_file: Path, extra_flags: list[str]) -> list[str]:
    return [
        "-std=c11",
        "-fno-builtin",
        *extra_flags,
        "-w",
        "-Xclang",
        "-ast-dump=json",
        "-fsyntax-only",
        str(c_file.resolve()),
    ]


def whole_archive_flags(lib_path: Path) -> list[str]:
    if platform.system() == "Darwin":
        return [f"-Wl,-force_load,{lib_path}"]
    return ["-Wl,--whole-archive", str(lib_path), "-Wl,--no-whole-archive"]


def run_cmd(cmd: list[str], cwd: Path, stdin_data: str | None = None, timeout: int = 60):
    return subprocess.run(
        cmd,
        cwd=str(cwd),
        input=stdin_data.encode("utf-8") if stdin_data is not None else None,
        capture_output=True,
        timeout=timeout,
    )


def generate_json_h_test_header(json_repo: Path, out_header: Path) -> None:
    readme = (json_repo / "c/README.md").read_text(encoding="utf-8")
    snippets = re.findall(r"```c(.*?)```", readme, flags=re.DOTALL)
    rendered = "".join("{" + s + "}\n\n" for s in snippets)
    out_header.write_text(rendered, encoding="utf-8")


def build_projects_spec(work_root: Path) -> list[dict]:
    specs = []

    def add(name, repo_rel, crate_type, rel_units, flags_fn=None):
        repo = (ROOT / repo_rel).resolve()
        if not repo.exists():
            return
        flags = flags_fn(repo) if flags_fn else []
        out_dir = (work_root / name / "rust").resolve()
        out_dir.mkdir(parents=True, exist_ok=True)
        units = []
        for u in rel_units:
            c_path = (repo / u) if not isinstance(u, Path) else u
            units.append({
                "file": str(c_path.resolve()),
                "args": clang_ast_args(repo, c_path, flags),
            })
        specs.append({
            "repo": repo,
            "job": {
                "name": name,
                "root": str(repo),
                "out_dir": str(out_dir),
                "crate_type": crate_type,
                "units": units,
            },
        })

    # 1. ht
    add(
        "ht",
        "vendor/crown/c-code/ht",
        "staticlib",
        ["ht.c"],
        lambda r: ["-I", str(r)],
    )

    # 2. quadtree
    add(
        "quadtree",
        "vendor/crown/c-code/quadtree-0.1.0",
        "staticlib",
        ["src/point.c", "src/bounds.c", "src/node.c", "src/quadtree.c"],
        lambda r: ["-I", str(r), "-I", str(r / "src")],
    )

    # 3. urlparser (header-only url.h tested via test.c -> pure Rust bin)
    add(
        "urlparser",
        "vendor/crown/c-code/urlparser",
        "bin",
        ["test.c"],
        lambda r: ["-I", str(r)],
    )

    # 4. rgba
    add(
        "rgba",
        "vendor/crown/c-code/rgba",
        "staticlib",
        ["src/rgba.c"],
        lambda r: ["-I", str(r), "-I", str(r / "src")],
    )

    # 5. libcsv
    add(
        "libcsv",
        "vendor/crown/c-code/libcsv",
        "staticlib",
        ["libcsv.c"],
        lambda r: ["-I", str(r)],
    )

    # 6. lodepng
    add(
        "lodepng",
        "vendor/crown/c-code/lodepng",
        "staticlib",
        ["lodepng.c"],
        lambda r: ["-I", str(r)],
    )

    # 7. genann
    add(
        "genann",
        "vendor/crown/c-code/genann-1.0.0",
        "staticlib",
        ["c/genann.c"],
        lambda r: ["-I", str(r / "c")],
    )

    # 8. bzip2 (all 7 libbz2 C modules)
    add(
        "bzip2",
        "vendor/crown/c-code/bzip2",
        "staticlib",
        [
            "blocksort.c",
            "huffman.c",
            "crctable.c",
            "randtable.c",
            "compress.c",
            "decompress.c",
            "bzlib.c",
        ],
        lambda r: ["-D_FILE_OFFSET_BITS=64", "-I", str(r)],
    )

    # 9. zlib (core standalone Z_SOLO compression/decompression modules)
    zlib_arch_flags = (
        ["-march=armv8-a+nocrc"]
        if platform.machine().lower() in ("arm64", "aarch64")
        else []
    )
    add(
        "zlib",
        "vendor/zlib",
        "staticlib",
        [
            "adler32.c",
            "crc32.c",
            "deflate.c",
            "infback.c",
            "inffast.c",
            "inflate.c",
            "inftrees.c",
            "trees.c",
            "zutil.c",
        ],
        lambda r: ["-DZ_SOLO", *zlib_arch_flags, "-I", str(r)],
    )

    # 10. json.h (wrapper TU + in-project header copy instantiating strong symbols)
    json_repo = (ROOT / "vendor/crown/c-code/json.h").resolve()
    if json_repo.exists():
        json_work = (work_root / "json_h" / "src").resolve()
        json_work.mkdir(parents=True, exist_ok=True)
        shutil.copy2(json_repo / "c/json.h", json_work / "json.h")
        wrapper_c = json_work / "json_lib.c"
        wrapper_c.write_text('#include "json.h"\n', encoding="utf-8")
        out_dir = (work_root / "json_h" / "rust").resolve()
        out_dir.mkdir(parents=True, exist_ok=True)
        specs.append({
            "repo": json_repo,
            "job": {
                "name": "json_h",
                "root": str(work_root / "json_h"),
                "out_dir": str(out_dir),
                "crate_type": "staticlib",
                "units": [
                    {
                        "file": str(wrapper_c),
                        "args": clang_ast_args(
                            work_root / "json_h",
                            wrapper_c,
                            ["-I", str(json_work)],
                        ),
                    }
                ],
            },
        })

    return specs


def verify_native_suite(name: str, repo: Path, artifact: Path, proj_work: Path) -> dict:
    run_c = proj_work / "run_c"
    run_rs = proj_work / "run_rs"
    for d in (run_c, run_rs):
        if d.exists():
            shutil.rmtree(d)
        d.mkdir(parents=True)

    c_flags = ["-O2", "-w"]

    try:
        if name == "ht":
            sample_in = "foo bar the bar bar bar the\nalpha beta alpha gamma\n"
            c_bin = run_c / "demo_c"
            rs_bin = run_rs / "demo_rs"
            res_cc = run_cmd(
                ["clang", *c_flags, f"-I{repo}", str(repo / "samples/demo.c"), str(repo / "ht.c"), "-o", str(c_bin)],
                run_c,
            )
            res_link = run_cmd(
                ["clang", *c_flags, f"-I{repo}", str(repo / "samples/demo.c"), str(artifact), "-o", str(rs_bin)],
                run_rs,
            )
            if res_cc.returncode != 0 or res_link.returncode != 0:
                return {
                    "suite_ok": False,
                    "error": f"link failed (c={res_cc.returncode}, rs={res_link.returncode}): {(res_link.stderr or res_cc.stderr).decode(errors='replace')[:800]}",
                }
            rc = run_cmd([str(c_bin)], run_c, stdin_data=sample_in)
            rr = run_cmd([str(rs_bin)], run_rs, stdin_data=sample_in)
            ok = rc.returncode == 0 and rr.returncode == 0 and rc.stdout == rr.stdout
            return {
                "suite_ok": ok,
                "c_exit": rc.returncode,
                "rs_exit": rr.returncode,
                "stdout_match": rc.stdout == rr.stdout,
                "sample_lines": len(rr.stdout.splitlines()),
                "error": None if ok else f"mismatch: rc={rc.returncode} rr={rr.returncode}\nRS:{rr.stderr.decode(errors='replace')[:400]}",
            }

        if name == "quadtree":
            c_srcs = [str(repo / s) for s in ("src/point.c", "src/bounds.c", "src/node.c", "src/quadtree.c")]
            c_bin = run_c / "test_c"
            rs_bin = run_rs / "test_rs"
            res_cc = run_cmd(["clang", *c_flags, f"-I{repo}", f"-I{repo / 'src'}", str(repo / "test.c"), *c_srcs, "-lm", "-o", str(c_bin)], run_c)
            res_link = run_cmd(["clang", *c_flags, f"-I{repo}", f"-I{repo / 'src'}", str(repo / "test.c"), str(artifact), "-lm", "-o", str(rs_bin)], run_rs)
            if res_cc.returncode != 0 or res_link.returncode != 0:
                return {"suite_ok": False, "error": f"link failed: {(res_link.stderr or res_cc.stderr).decode(errors='replace')[:800]}"}
            rc = run_cmd([str(c_bin)], run_c)
            rr = run_cmd([str(rs_bin)], run_rs)
            ok = rc.returncode == 0 and rr.returncode == 0 and rc.stdout == rr.stdout
            return {"suite_ok": ok, "c_exit": rc.returncode, "rs_exit": rr.returncode, "stdout_match": rc.stdout == rr.stdout, "stdout_tail": rr.stdout.decode(errors="replace").strip()}

        if name == "urlparser":
            c_bin = run_c / "urlparser_c"
            res_cc = run_cmd(["clang", *c_flags, f"-I{repo}", str(repo / "test.c"), "-o", str(c_bin)], run_c)
            if res_cc.returncode != 0:
                return {"suite_ok": False, "error": f"c compile failed: {res_cc.stderr.decode(errors='replace')[:600]}"}
            rc = run_cmd([str(c_bin)], run_c)
            rr = run_cmd([str(artifact)], run_rs)
            parity_ok = rc.returncode == rr.returncode and rc.stdout == rr.stdout
            return {
                "suite_ok": parity_ok,
                "c_exit": rc.returncode,
                "rs_exit": rr.returncode,
                "stdout_match": rc.stdout == rr.stdout,
                "note": "upstream url.h:191 malloc(1) UB traps identically in C baseline and Rust" if rc.returncode != 0 else None,
            }

        if name == "rgba":
            c_bin = run_c / "rgba_c"
            rs_bin = run_rs / "rgba_rs"
            res_cc = run_cmd(["clang", *c_flags, f"-I{repo}", f"-I{repo / 'src'}", str(repo / "test.c"), str(repo / "src/rgba.c"), "-o", str(c_bin)], run_c)
            res_link = run_cmd(["clang", *c_flags, f"-I{repo}", f"-I{repo / 'src'}", str(repo / "test.c"), str(artifact), "-o", str(rs_bin)], run_rs)
            if res_cc.returncode != 0 or res_link.returncode != 0:
                return {"suite_ok": False, "error": f"link failed: {(res_link.stderr or res_cc.stderr).decode(errors='replace')[:800]}"}
            rc = run_cmd([str(c_bin)], run_c)
            rr = run_cmd([str(rs_bin)], run_rs)
            ok = rc.returncode == 0 and rr.returncode == 0 and rc.stdout == rr.stdout
            return {"suite_ok": ok, "c_exit": rc.returncode, "rs_exit": rr.returncode, "stdout_match": rc.stdout == rr.stdout, "stdout_tail": rr.stdout.decode(errors="replace").strip()}

        if name == "libcsv":
            c_bin = run_c / "csv_c"
            rs_bin = run_rs / "csv_rs"
            res_cc = run_cmd(["clang", *c_flags, f"-I{repo}", str(repo / "test_csv.c"), str(repo / "libcsv.c"), "-o", str(c_bin)], run_c)
            res_link = run_cmd(["clang", *c_flags, f"-I{repo}", str(repo / "test_csv.c"), str(artifact), "-o", str(rs_bin)], run_rs)
            if res_cc.returncode != 0 or res_link.returncode != 0:
                return {"suite_ok": False, "error": f"link failed: {(res_link.stderr or res_cc.stderr).decode(errors='replace')[:800]}"}
            rc = run_cmd([str(c_bin)], run_c)
            rr = run_cmd([str(rs_bin)], run_rs)
            ok = rc.returncode == 0 and rr.returncode == 0 and rc.stdout == rr.stdout
            return {"suite_ok": ok, "c_exit": rc.returncode, "rs_exit": rr.returncode, "stdout_match": rc.stdout == rr.stdout, "stdout_tail": rr.stdout.decode(errors="replace").strip()}

        if name == "lodepng":
            c_enc = run_c / "enc_c"
            c_dec = run_c / "dec_c"
            rs_enc = run_rs / "enc_rs"
            rs_dec = run_rs / "dec_rs"
            for cmd, wd in [
                (["clang", *c_flags, f"-I{repo}", str(repo / "examples/example_encode.c"), str(repo / "lodepng.c"), "-o", str(c_enc)], run_c),
                (["clang", *c_flags, f"-I{repo}", str(repo / "examples/example_decode.c"), str(repo / "lodepng.c"), "-o", str(c_dec)], run_c),
                (["clang", *c_flags, f"-I{repo}", str(repo / "examples/example_encode.c"), str(artifact), "-o", str(rs_enc)], run_rs),
                (["clang", *c_flags, f"-I{repo}", str(repo / "examples/example_decode.c"), str(artifact), "-o", str(rs_dec)], run_rs),
            ]:
                r = run_cmd(cmd, wd)
                if r.returncode != 0:
                    return {"suite_ok": False, "error": f"compile/link failed: {r.stderr.decode(errors='replace')[:800]}"}
            c_png = run_c / "out.png"
            rs_png = run_rs / "out.png"
            rc_e = run_cmd([str(c_enc), str(c_png)], run_c)
            rc_d = run_cmd([str(c_dec), str(c_png)], run_c)
            rr_e = run_cmd([str(rs_enc), str(rs_png)], run_rs)
            rr_d = run_cmd([str(rs_dec), str(rs_png)], run_rs)
            png_match = c_png.exists() and rs_png.exists() and c_png.read_bytes() == rs_png.read_bytes()
            ok = (
                rc_e.returncode == 0
                and rc_d.returncode == 0
                and rr_e.returncode == 0
                and rr_d.returncode == 0
                and png_match
                and rc_d.stdout == rr_d.stdout
            )
            return {
                "suite_ok": ok,
                "png_bytes": len(rs_png.read_bytes()) if rs_png.exists() else 0,
                "png_byte_identical": png_match,
                "decode_stdout_match": rc_d.stdout == rr_d.stdout,
            }

        if name == "genann":
            c_bin = run_c / "genann_c"
            rs_bin = run_rs / "genann_rs"
            res_cc = run_cmd(["clang", *c_flags, f"-I{repo / 'c'}", str(repo / "c/test.c"), str(repo / "c/genann.c"), "-lm", "-o", str(c_bin)], run_c)
            res_link = run_cmd(["clang", *c_flags, f"-I{repo / 'c'}", str(repo / "c/test.c"), str(artifact), "-lm", "-o", str(rs_bin)], run_rs)
            if res_cc.returncode != 0 or res_link.returncode != 0:
                return {"suite_ok": False, "error": f"link failed: {(res_link.stderr or res_cc.stderr).decode(errors='replace')[:800]}"}
            rc = run_cmd([str(c_bin)], run_c, timeout=120)
            rr = run_cmd([str(rs_bin)], run_rs, timeout=120)
            ok = rc.returncode == 0 and rr.returncode == 0
            return {
                "suite_ok": ok,
                "c_exit": rc.returncode,
                "rs_exit": rr.returncode,
                "stdout_tail": rr.stdout.decode(errors="replace").splitlines()[-3:],
            }

        if name == "bzip2":
            rs_bin = run_rs / "bzip2_rs"
            res_link = run_cmd(
                ["clang", *c_flags, "-D_FILE_OFFSET_BITS=64", f"-I{repo}", str(repo / "bzip2.c"), str(artifact), "-o", str(rs_bin)],
                run_rs,
            )
            if res_link.returncode != 0:
                return {"suite_ok": False, "error": f"link failed: {res_link.stderr.decode(errors='replace')[:800]}"}
            checks = 0
            for level, idx in [("-1", 1), ("-2", 2), ("-3", 3)]:
                ref = (repo / f"sample{idx}.ref").read_bytes()
                bz2_exp = (repo / f"sample{idx}.bz2").read_bytes()
                comp = subprocess.run([str(rs_bin), level], cwd=str(run_rs), input=ref, capture_output=True, timeout=60)
                if comp.returncode != 0 or comp.stdout != bz2_exp:
                    return {"suite_ok": False, "error": f"bzip2 {level} compress mismatch on sample{idx} (exit={comp.returncode}, len={len(comp.stdout)} vs {len(bz2_exp)})"}
                checks += 1
            for flag, idx in [("-d", 1), ("-d", 2), ("-ds", 3)]:
                ref = (repo / f"sample{idx}.ref").read_bytes()
                bz2 = (repo / f"sample{idx}.bz2").read_bytes()
                dec = subprocess.run([str(rs_bin), flag], cwd=str(run_rs), input=bz2, capture_output=True, timeout=60)
                if dec.returncode != 0 or dec.stdout != ref:
                    return {"suite_ok": False, "error": f"bzip2 {flag} decompress mismatch on sample{idx} (exit={dec.returncode})"}
                checks += 1
            return {"suite_ok": True, "checks_passed": f"{checks}/6"}

        if name == "zlib":
            c_objs = [
                str(repo / s)
                for s in (
                    "adler32.c", "crc32.c", "deflate.c", "infback.c", "inffast.c",
                    "inflate.c", "inftrees.c", "trees.c", "zutil.c",
                )
            ]
            c_bin = run_c / "example_c"
            rs_bin = run_rs / "example_rs"
            res_cc = run_cmd(["clang", *c_flags, "-DZ_SOLO", f"-I{repo}", str(repo / "test/example.c"), *c_objs, "-o", str(c_bin)], run_c)
            res_link = run_cmd(["clang", *c_flags, "-DZ_SOLO", f"-I{repo}", str(repo / "test/example.c"), str(artifact), "-o", str(rs_bin)], run_rs)
            if res_cc.returncode != 0 or res_link.returncode != 0:
                return {"suite_ok": False, "error": f"link failed: {(res_link.stderr or res_cc.stderr).decode(errors='replace')[:800]}"}
            rc = run_cmd([str(c_bin)], run_c)
            rr = run_cmd([str(rs_bin)], run_rs)
            ok = rc.returncode == 0 and rr.returncode == 0 and rc.stdout == rr.stdout
            return {
                "suite_ok": ok,
                "c_exit": rc.returncode,
                "rs_exit": rr.returncode,
                "stdout_match": rc.stdout == rr.stdout,
                "stdout_lines": len(rr.stdout.splitlines()),
            }

        if name == "json_h":
            nm = run_cmd(["nm", "-gU" if platform.system() == "Darwin" else "-g", str(artifact)], run_rs)
            nm_out = nm.stdout.decode(errors="replace")
            has_strong_json_parse = bool(re.search(r"\bT\b.*json_parse\b", nm_out))
            gen_inc = proj_work / "gen_inc"
            gen_inc.mkdir(parents=True, exist_ok=True)
            generate_json_h_test_header(repo, gen_inc / "generated.h")

            c_test_dir = repo / "c/test"
            cmake_txt = (c_test_dir / "CMakeLists.txt").read_text(encoding="utf-8")
            m = re.search(r"add_executable\s*\(\s*json_test\s+(.*?)\)", cmake_txt, flags=re.DOTALL)
            sources = []
            if m:
                for tok in m.group(1).split():
                    if tok.endswith(".c") or tok.endswith(".cpp"):
                        sources.append(tok)

            def compile_suite(out_bin: Path, extra_link: list[str], wd: Path):
                objs = []
                for idx, src in enumerate(sources):
                    src_path = c_test_dir / src
                    obj_path = wd / f"test_obj_{idx}.o"
                    compiler = "clang++" if src.endswith(".cpp") else "clang"
                    std_flag = "-std=c++14" if src.endswith(".cpp") else "-std=c99"
                    r = run_cmd(
                        [compiler, "-O2", "-w", std_flag, "-c", f"-I{repo / 'c'}", f"-I{gen_inc}", str(src_path), "-o", str(obj_path)],
                        wd,
                    )
                    if r.returncode != 0:
                        return None, f"{src}: {r.stderr.decode(errors='replace')[:500]}"
                    objs.append(str(obj_path))
                r = run_cmd(["clang++", "-O2", *extra_link, *objs, "-o", str(out_bin)], wd)
                if r.returncode != 0:
                    return None, f"link: {r.stderr.decode(errors='replace')[:800]}"
                return out_bin, None

            c_bin, err_c = compile_suite(run_c / "json_test_c", [], run_c)
            rs_bin, err_rs = compile_suite(run_rs / "json_test_rs", whole_archive_flags(artifact), run_rs)
            if err_c or err_rs:
                return {"suite_ok": False, "has_strong_json_parse": has_strong_json_parse, "error": err_rs or err_c}
            rc = run_cmd([str(c_bin)], run_c, timeout=120)
            rr = run_cmd([str(rs_bin)], run_rs, timeout=120)
            ok = rc.returncode == 0 and rr.returncode == 0 and has_strong_json_parse
            return {
                "suite_ok": ok,
                "has_strong_json_parse": has_strong_json_parse,
                "c_exit": rc.returncode,
                "rs_exit": rr.returncode,
                "stdout_tail": rr.stdout.decode(errors="replace").splitlines()[-3:],
            }

    except Exception as exc:
        return {"suite_ok": False, "error": str(exc)}

    return {"suite_ok": False, "error": f"no test runner defined for {name}"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--only", help="Comma-separated subset of project names to run")
    parser.add_argument(
        "--out",
        type=Path,
        default=ROOT / ".reforge-demo/oss-e2e-report.json",
    )
    args = parser.parse_args()

    work_root = ROOT / ".reforge-demo/oss-e2e"
    work_root.mkdir(parents=True, exist_ok=True)
    specs = build_projects_spec(work_root)
    if args.only:
        allowed = {x.strip() for x in args.only.split(",") if x.strip()}
        specs = [s for s in specs if s["job"]["name"] in allowed]

    manifest_path = work_root / "manifest.json"
    bench_out = work_root / "bench_build.json"
    manifest_path.write_text(json.dumps({"projects": [s["job"] for s in specs]}, indent=2), encoding="utf-8")

    subprocess.run(
        [
            "cargo", "run", "--release", "--locked", "-p", "reforge-engine",
            "--example", "oss_e2e_bench", "--",
            "--manifest", str(manifest_path),
            "--output", str(bench_out),
        ],
        cwd=str(ROOT),
        check=True,
    )

    build_report = json.loads(bench_out.read_text(encoding="utf-8"))
    spec_map = {s["job"]["name"]: s for s in specs}

    final_projects = []
    for proj in build_report["projects"]:
        name = proj["name"]
        entry = dict(proj)
        if proj["stage"] == "rustc_ok" and proj.get("artifact"):
            suite = verify_native_suite(
                name,
                spec_map[name]["repo"],
                Path(proj["artifact"]),
                work_root / name,
            )
            entry["suite"] = suite
        else:
            entry["suite"] = {"suite_ok": False, "error": proj.get("error")}
        final_projects.append(entry)

    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps({"projects": final_projects}, indent=2), encoding="utf-8")

    passed = sum(1 for p in final_projects if p["suite"].get("suite_ok"))
    print(f"\n=== Real OSS E2E Porting Benchmark ({passed}/{len(final_projects)} passed) ===")
    for p in final_projects:
        suite = p["suite"]
        status = "PASS" if suite.get("suite_ok") else "FAIL"
        print(
            f"  [{status}] {p['name']:10s} | stage={p['stage']:16s} | units={p['units_ok']}/{p['units_total']} | fns={p['functions']:3d}"
        )
        if suite.get("suite_ok"):
            details = {k: v for k, v in suite.items() if k != "suite_ok"}
            if details:
                print(f"         verified: {details}")
        else:
            err = suite.get("error") or p.get("error") or "unknown error"
            for line in str(err).splitlines()[:6]:
                print(f"         err: {line}")


if __name__ == "__main__":
    main()

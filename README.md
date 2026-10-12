# Reforge-MCP

**Linux 커널 규모의 시스템 소프트웨어를 Rust로 자동 포팅하는 MCP 기반 분석·생성·검증 인프라**를 목표로 합니다. C 시스템 소프트웨어를 우선하며 C++·Java·Python·Go 확장도 유지합니다. 기존 프로젝트 이름은 Fossil-MCP입니다.

## 제품 방향

최종 목표는 커널의 빌드 구성·ABI·메모리 수명·동시성·실행 동작을 유지하는 Rust 구현을 자동으로 산출하는 것입니다. 현재 커널 포팅 지원은 구현되지 않았습니다. 작은 함수 변환기는 초기 검증 백엔드이며, 커널 규모의 완료 기준은 [커널 포팅 아키텍처와 로드맵](docs/kernel-scale-roadmap.md)을 따릅니다.

AI 에이전트가 원본을 분석하고 Rust 후보를 작성할 수 있도록 MCP 서버가 분석 근거, 의존성 순서, 작업 상태, 컴파일 진단, 동작 비교 결과를 제공합니다. 최종 목표는 이 과정을 자동으로 반복하는 포팅 파이프라인입니다.

```text
원본 프로젝트 → 분석 → 포팅 계획 → Rust 후보 생성
                                  ↑          ↓
                              수정 피드백 ← 검증
                                             ↓
                               검증 결과와 미지원 항목 보고
```

첫 구현은 Clang AST를 읽어 제한된 C 함수를 Rust로 직접 변환하는 생성기입니다. AI 에이전트가 후보를 제출하는 경로도 제공합니다. Joern은 후속 분석 백엔드로 연결할 계획입니다.

다음 우선순위는 **Kbuild/`.config`/compilation database 기반 빌드 컨텍스트 수집 → typed IR → C/Rust 혼합 빌드 및 ABI → 커널 빌드·부팅·시험 worker**입니다. 함수 단위 테스트를 커널 준비도나 전체 시스템 동등성으로 보고하지 않습니다.

## 현재 구현 상태

| 기능 | 상태 |
|---|---|
| C 분석과 소스 스냅샷 | Clang AST, 파일/내용 해시, 헤더·매크로 전처리, 미지원 구문 거부 |
| 포팅 계획과 컨텍스트 | 함수 의존성 순서, 재귀 그룹, 원본 범위와 Rust 대응 |
| Rust 자동 생성 | 제한된 `int32` C 함수를 dependency-free Cargo 라이브러리로 변환 |
| AI 후보 제출 | `src/lib.rs`를 새 시도로 저장; 자동 생성 후보와 같은 검증 경로 사용 |
| 원본/Rust 차등 검증 | C UBSan, offline Cargo 빌드, 동일 입력의 stdout/stderr/종료 코드 비교 |
| 작업 저장과 재개 | SQLite 작업/이벤트, 생성·검증 이력, 소스/후보 변경 시 검증 만료 |
| 저장소 검색과 일반 로드맵 | 기존 구현 유지 |
| C++·Java·Python·Go 포팅, Joern, 모델 기반 자동 수정 | 계획 |

현재 지원 범위는 `int` 인자/반환값을 사용하는 C 라이브러리 함수입니다. 산술·비교·논리·비트 연산, 분기, 초기화된 지역 변수, 직접 내부 호출과 재귀를 지원합니다. include guard, 매크로, 조건부 컴파일, `int` 함수 선언과 공유 inline 함수를 담은 헤더도 지원합니다. 포인터·전역 상태·루프·외부 함수 호출·`main`은 거부합니다. 프로젝트 한도는 C/헤더 32파일, 총 256 KiB입니다. 시스템 헤더의 선언은 원본 함수로 잘못 변환하지 않으며, 프로젝트/외부 include 파일의 해시를 기록해 변경 시 검증을 만료시킵니다.

`completed`는 **선택한 진입 함수와 제공한 fixture가 최신 소스/후보에서 일치했다**는 뜻입니다. 전체 프로그램 동등성이나 모든 입력에 대한 보장은 아닙니다. C 원본의 UB, 빌드 실패, 타임아웃은 검증 성공으로 처리하지 않습니다.

## 포팅 실행 흐름

1. `analyze_porting_project({"source_path":"/absolute/path/to/c-library"})`
2. 반환된 `job_id`로 `create_porting_plan({"job_id":"..."})`
3. `generate_rust({"job_id":"..."})`로 자동 생성하거나, `get_porting_context`를 읽고 `submit_rust_candidate`로 Rust 후보 제출
4. `verify_port({"job_id":"...","entry_function":"score","cases":[[-7,3],[0,0],[200,10]]})`
5. `get_porting_report({"job_id":"..."})`로 검증 결과·로그·아티팩트 경로 확인

재시작 후에는 `list_porting_jobs({})`로 작업을 찾아 보고서를 읽습니다. 실패한 후보를 수정해 제출하면 새 생성 시도가 생기며 이전 검증 기록은 유지됩니다. 원본이 바뀌면 새 분석 작업을 만들어야 합니다.

| 도구 | 역할 |
|---|---|
| `get_porting_capabilities` | 구현 범위·필수 도구·실행 한계 조회 |
| `analyze_porting_project` | 스냅샷 생성 및 Clang 분석 |
| `create_porting_plan` | 의존성 순서와 검증 계약 생성 |
| `get_porting_context` | 함수별 원본·타입·호출·Rust 대응 조회 |
| `generate_rust` | 결정적 Rust 생성 및 Cargo 프로젝트 작성 |
| `submit_rust_candidate` | 에이전트가 작성한 `src/lib.rs` 저장 |
| `verify_port` | 원본/Rust 빌드·실행 비교와 evidence 저장 |
| `get_porting_report` | 현재 상태, 검증 이력, 변경 감지 조회 |
| `list_porting_jobs` | 최근 100개 저장 작업 조회 |

검증은 로컬 호스트에서 코드를 실행합니다. 명령은 셸 없이 실행하고 컴파일/실행 시간과 출력량을 제한하며 Unix에서는 프로세스 그룹을 정리합니다. 현재 OS 격리 sandbox는 제공하지 않습니다. 생성 시도는 작업당 32회, 검증은 64회, 한 검증의 fixture는 1–256개로 제한합니다. 분석/검증 한 번의 전체 시간은 120초, 각 fixture 실행은 10초, 프로세스별 stdout/stderr는 각각 8 MiB로 제한합니다.

## 시작하기

서버 빌드는 Rust 1.88 이상이 필요합니다. 포팅 분석/검증에는 PATH의 `clang`(JSON AST 및 UBSan 지원), `rustc`, `cargo`가 필요합니다. 내부 `fossil-*` 크레이트 이름은 기존 이름을 사용합니다.

```bash
git clone https://github.com/sjkim1127/reforge-mcp
cd reforge-mcp
cargo build --release --locked --bin reforge-mcp
```

```json
{
  "mcpServers": {
    "reforge-mcp": {
      "command": "/absolute/path/to/reforge-mcp/target/release/reforge-mcp"
    }
  }
}
```

첫 호출은 `get_porting_capabilities({})`입니다. 기존 검색은 `clone_reference → index_repo → locate_implementation → get_symbol_source` 순서로 사용합니다.

포팅 데이터는 `REFORGE_DATA_DIR` 아래 `porting.db`와 `jobs/<job_id>/`에 저장됩니다. 기본값은 OS 캐시 디렉터리의 `reforge-mcp`입니다. 생성 결과는 `candidates/<attempt>/Cargo.toml`과 `src/lib.rs`, 검증 로그는 `verification/<attempt>/logs/`에서 확인할 수 있습니다. 기존 검색 캐시와 포팅 작업 저장소는 분리되어 있습니다.

## 재현 가능한 MCP 데모

```bash
cargo build --locked --bin reforge-mcp
python3 scripts/porting_smoke.py
```

실제 stdio MCP 연결을 통해 2개의 C 파일·3개 함수로 구성된 fixture를 Rust로 변환하고 6개 입력을 비교합니다. 서버 재시작 후 결과 재조회와 포인터 입력의 거부도 검사합니다. `.reforge-demo/report.json`에 실행 근거를 저장합니다. 추가로 헤더/매크로 프로젝트 6개 입력과, 커밋을 고정한 실제 [musl `abs.c`](fixtures/porting/musl-abs/PROVENANCE.md) 6개 입력을 검증합니다. `INT_MIN`에서 musl 원본의 UB를 검출해 완료를 차단하는 것도 확인합니다. 결과는 `.reforge-demo/headers-report.json`, `.reforge-demo/musl-abs-report.json`에 저장합니다. musl 검증은 실제 함수 하나에 한정되며 전체 musl 포팅이나 외부 프로젝트 corpus 수용 검증은 아닙니다.

## 전환 및 호환성

실행 파일 이름은 `fossil-mcp`에서 `reforge-mcp`로 변경됩니다. 기존 MCP 설정의 command 경로를 갱신해야 합니다. 릴리즈 아카이브도 새 이름을 사용합니다. 기존 로컬 데이터 재사용을 위해 `FOSSIL_CACHE_DIR`, `FOSSIL_LOG`와 SQLite 저장 경로는 유지합니다. GitHub 저장소는 `sjkim1127/reforge-mcp`로 변경했습니다. 기존 로컬 체크아웃 폴더는 그대로 사용해도 됩니다.

## 설계와 개발

- [커널 규모 포팅 목표와 아키텍처](docs/kernel-scale-roadmap.md)
- [현재 엔진 설계 및 단계별 완료 기준](docs/rust-porting-design.md)
- [기존 도구 계약과 운영 참고](docs/fossil-reference.md): 전환 전 문서이며 실행 파일 이름은 위 설정을 따릅니다.
- [릴리즈 절차](RELEASING.md)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
```

Apache-2.0 licensed. See [LICENSE](LICENSE).

# Reforge-MCP

**C·C++·Java·Python·Go 프로그램과 의존 생태계를 Rust로 자동 이전하는 Agent용 MCP 인프라**를 목표로 합니다. Linux 커널, Ghidra, Spring Boot는 대표 검증 대상이며 프로젝트 규모의 상한을 두는 기준이 아닙니다. 기존 프로젝트 이름은 Fossil-MCP입니다.

## 제품 방향

원본 코드의 기계적 변환과 라이브러리·프레임워크 대체를 하나의 이전 작업 그래프로 연결합니다. Agent는 실제로 사용된 API와 동작 계약을 분석하고, Rust 후보를 찾아 직접 대체·어댑터·여러 라이브러리 조합·의존성 자체 포팅 중 적절한 경로를 선택한 뒤 실행 검증을 수행합니다. 같은 기능의 Rust 라이브러리가 없으면 해당 의존성도 별도 포팅 작업이 됩니다.

전체 프로그램과 생태계의 완전 자동 이전은 장기 제품 목표입니다. 현재 실행 가능한 구현은 제한된 C 백엔드와 캠페인·의존성 계약 검증 기반입니다. 최상위 아키텍처와 우선순위는 [프로그램·생태계 이전 설계](docs/ecosystem-migration-design.md)를 따르며, [커널 로드맵](docs/kernel-scale-roadmap.md)은 시스템 프로파일의 설계입니다.

AI 에이전트가 원본을 분석하고 Rust 후보를 작성할 수 있도록 MCP 서버가 분석 근거, 의존성 순서, 작업 상태, 컴파일 진단, 동작 비교 결과를 제공합니다. 최종 목표는 이 과정을 자동으로 반복하는 포팅 파이프라인입니다.

```text
원본 프로젝트 → 분석 → 포팅 계획 → Rust 후보 생성
                                  ↑          ↓
                              수정 피드백 ← 검증
                                             ↓
                               검증 결과와 미지원 항목 보고
```

첫 구현은 Clang AST를 읽어 제한된 C 함수를 Rust로 직접 변환하는 생성기입니다. AI 에이전트가 후보를 제출하는 경로도 제공합니다. Joern은 후속 분석 백엔드로 연결할 계획입니다.

캠페인 인벤토리와 Agent의 의존성 티켓·후보·어댑터 계획·계약 검증 수명주기를 구현했습니다. 다음 우선순위는 **빌드/패키지 수집기 → 공통 타입/동작 모델 → 프로파일별 빌드/실행 worker → 자동 반복**입니다. 큰 프로젝트는 모듈과 단위 작업으로 나눠 저장·증분 분석·재개하며, 함수 테스트나 crate 검색 결과만으로 전체 이전 완료를 보고하지 않습니다.

## 현재 구현 상태

| 기능 | 상태 |
|---|---|
| C 분석과 소스 스냅샷 | Clang AST, 파일/내용 해시, 헤더·매크로 전처리, 미지원 구문 거부 |
| 포팅 계획과 컨텍스트 | 함수 의존성 순서, 재귀 그룹, 원본 범위와 Rust 대응 |
| Rust 자동 생성 | 제한된 `int32` C 함수를 dependency-free Cargo 라이브러리로 변환 |
| AI 후보 제출 | `src/lib.rs`를 새 시도로 저장; 자동 생성 후보와 같은 검증 경로 사용 |
| 원본/Rust 차등 검증 | C UBSan, offline Cargo 빌드, 동일 입력의 stdout/stderr/종료 코드 비교 |
| 작업 저장과 재개 | SQLite 작업/이벤트, 생성·검증 이력, 소스/후보 변경 시 검증 만료 |
| 캠페인 인벤토리 | 파일 해시, 언어/빌드 manifest 발견, 페이지 조회, 소스 컨텍스트 |
| 의존성 이전 수명주기 | Agent 작성 계약·후보, 어댑터 계획, 하위 캠페인 연결, JSON 실행 차등 검증 |
| 저장소 검색과 일반 로드맵 | 기존 구현 유지 |
| C++·Java·Python·Go 포팅, Joern, 모델 기반 자동 수정 | 계획 |

현재 지원 범위는 `int` 인자/반환값을 사용하는 C 라이브러리 함수입니다. 산술·비교·논리·비트 연산, 분기, 초기화된 지역 변수, 직접 내부 호출과 재귀를 지원합니다. include guard, 매크로, 조건부 컴파일, `int` 함수 선언과 공유 inline 함수를 담은 헤더도 지원합니다. 포인터·전역 상태·루프·외부 함수 호출·`main`은 거부합니다. 프로젝트 한도는 C/헤더 32파일, 총 256 KiB입니다. 시스템 헤더의 선언은 원본 함수로 잘못 변환하지 않으며, 프로젝트/외부 include 파일의 해시를 기록해 변경 시 검증을 만료시킵니다.

`completed`는 **선택한 진입 함수와 제공한 fixture가 최신 소스/후보에서 일치했다**는 뜻입니다. 전체 프로그램 동등성이나 모든 입력에 대한 보장은 아닙니다. C 원본의 UB, 빌드 실패, 타임아웃은 검증 성공으로 처리하지 않습니다.

## 캠페인과 의존성 이전 흐름

1. `inventory_project`로 프로젝트를 등록하고 `get_migration_inventory`, `get_migration_source_context`로 파일과 실제 API 사용을 조사합니다.
2. `register_dependency_requirements`로 의존성 버전, 사용하는 API, 소스 줄 범위, 미확인 사용과 필수 동작 계약을 등록합니다.
3. Agent가 Rust 후보를 조사하고 구현한 뒤 `record_dependency_candidate`로 버전·문서 URL·라이선스·타깃·API 대응·누락 기능을 제출합니다. `plan_dependency_adapter`는 계획을 저장하며 코드를 생성하지 않습니다.
4. 후보가 없으면 `create_dependency_campaign`으로 의존성 자체의 포팅 작업을 연결합니다. 직접 대체, 어댑터, 여러 Rust 라이브러리 조합, 의존성 포팅, 임시 FFI 전략을 기록할 수 있습니다.
5. Agent가 원본/후보 실행 파일 또는 launcher를 **각 프로젝트 내부에 준비**합니다. `verify_dependency_replacement`는 같은 fixture argv로 실행하고 비어 있지 않은 JSON 객체/배열, stderr, 종료 코드를 비교합니다. 모든 필수 계약에 사례가 있어야 하며 미확인 사용·누락 기능·API 대응 누락은 수용을 차단합니다.
6. `get_dependency_requirements`, `get_migration_campaign`, `next_migration_tasks`로 근거와 다음 작업을 읽습니다. `revise_dependency_requirements`는 수정할 때마다 이전 수용 근거를 만료시킵니다.

`accepted_fixture_scope`는 제출된 계약 사례의 통과입니다. 임시 FFI는 `verified_temporary_ffi`로 별도 표시합니다. 원본·후보·실행 파일 변경 및 중단된 검증은 수용 근거가 되지 않습니다. 의존성이 통과해도 `program_migration_completed`는 `false`이며 프로그램 변환·통합이 남습니다.

현재 인벤토리는 파일/manifest 발견이며 Maven/Gradle 패키지 해석이나 API 사용 자동 분석을 수행하지 않습니다. 후보 URL/라이선스/버전과 사용 범위는 Agent가 제출한 정보입니다. 서버 내부 registry 검색, 자동 어댑터 생성, 의존성 probe 빌드, 동적 런타임 환경 fingerprint는 아직 구현 전입니다. 이 API의 실행 계약과 한도는 [캠페인 가이드](docs/migration-campaigns.md)를 참고합니다.

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
python3 scripts/campaign_smoke.py
```

실제 stdio MCP 연결을 통해 2개의 C 파일·3개 함수로 구성된 fixture를 Rust로 변환하고 6개 입력을 비교합니다. 서버 재시작 후 결과 재조회와 포인터 입력의 거부도 검사합니다. `.reforge-demo/report.json`에 실행 근거를 저장합니다. 추가로 헤더/매크로 프로젝트 6개 입력과, 커밋을 고정한 실제 [musl `abs.c`](fixtures/porting/musl-abs/PROVENANCE.md) 6개 입력을 검증합니다. `INT_MIN`에서 musl 원본의 UB를 검출해 완료를 차단하는 것도 확인합니다. 결과는 `.reforge-demo/headers-report.json`, `.reforge-demo/musl-abs-report.json`에 저장합니다. musl 검증은 실제 함수 하나에 한정되며 전체 musl 포팅이나 외부 프로젝트 corpus 수용 검증은 아닙니다.

캠페인 데모는 실제 C `strtol`과 Rust 표준 라이브러리 파싱을 비교합니다. 공백·오류 처리 계약을 빠뜨린 후보의 실패, 작성된 어댑터의 8개 입력 통과, 계약 수정 후 검증 만료 및 재시작 후 조회를 확인합니다. `.reforge-demo/campaign-report.json`에 저장됩니다. probe와 어댑터는 사람이 작성한 fixture이며 자동 의존성 포팅 결과는 아닙니다.

## 전환 및 호환성

실행 파일 이름은 `fossil-mcp`에서 `reforge-mcp`로 변경됩니다. 기존 MCP 설정의 command 경로를 갱신해야 합니다. 릴리즈 아카이브도 새 이름을 사용합니다. 기존 로컬 데이터 재사용을 위해 `FOSSIL_CACHE_DIR`, `FOSSIL_LOG`와 SQLite 저장 경로는 유지합니다. GitHub 저장소는 `sjkim1127/reforge-mcp`로 변경했습니다. 기존 로컬 체크아웃 폴더는 그대로 사용해도 됩니다.

## 설계와 개발

- [프로그램·생태계 Rust 이전과 Agent 의존성 해결](docs/ecosystem-migration-design.md)
- [시스템/커널 전용 프로파일](docs/kernel-scale-roadmap.md)
- [현재 엔진 설계 및 단계별 완료 기준](docs/rust-porting-design.md)
- [기존 도구 계약과 운영 참고](docs/fossil-reference.md): 전환 전 문서이며 실행 파일 이름은 위 설정을 따릅니다.
- [릴리즈 절차](RELEASING.md)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
```

Apache-2.0 licensed. See [LICENSE](LICENSE).

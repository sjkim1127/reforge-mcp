# Reforge-MCP

C · C++ · Java · Python · Go 프로젝트를 Rust로 자동 포팅하고, 원본과의 동작 비교로 결과를 검증하는 MCP 서버를 목표로 합니다. 기존 프로젝트 이름은 Fossil-MCP입니다.

## 제품 방향

AI 에이전트가 원본을 분석하고 Rust 후보를 작성할 수 있도록 MCP 서버가 분석 근거, 의존성 순서, 작업 상태, 컴파일 진단, 동작 비교 결과를 제공합니다. 최종 목표는 이 과정을 자동으로 반복하는 포팅 파이프라인입니다.

```text
원본 프로젝트 → 분석 → 포팅 계획 → Rust 후보 생성
                                  ↑          ↓
                              수정 피드백 ← 검증
                                             ↓
                               검증 결과와 미지원 항목 보고
```

Joern은 선택 가능한 정적 분석 백엔드로 설계합니다. CPG에서 함수·호출·타입·제어/데이터 흐름을 추출하고, 포팅에 필요한 컨텍스트로 정규화합니다. 코드 생성은 AI 에이전트 또는 별도 생성 어댑터가 담당합니다. [Joern 공식 문서](https://docs.joern.io/)

## 현재 구현 상태

| 기능 | 상태 |
|---|---|
| `reforge-mcp` stdio 서버 및 제품 이름 | 구현 |
| `get_porting_capabilities` | 구현: 현재 기능과 계획을 JSON으로 구분 |
| 저장소 복제·검색·소스 조회 | 기존 구현 재사용 |
| SQLite 목표·로드맵·의존성·변경 이력 | 기존 구현 재사용 |
| C/C++·Java·Go 소스 의미 분석 | 계획 |
| Joern 연동 및 공통 포팅 모델 | 계획 |
| Rust 코드 자동 생성 | 계획 |
| 원본/Rust 차등 테스트 및 자동 수정 | 계획 |

기존 소스 검색 파서는 Rust, Python, TypeScript/JavaScript를 지원합니다. Python 검색 지원도 Python→Rust 포팅 지원을 의미하지 않습니다. 현재 서버로 다섯 언어의 자동 포팅을 수행할 수는 없습니다.

## 시작하기

Rust 1.88 이상이 필요합니다. 내부 `fossil-*` 크레이트 이름은 기존 이름을 사용합니다.

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

첫 호출은 `get_porting_capabilities({})`입니다. 계획에 기재된 도구는 아직 호출할 수 없습니다. 기존 검색은 `clone_reference → index_repo → locate_implementation → get_symbol_source` 순서로 사용합니다.

## 전환 및 호환성

실행 파일 이름은 `fossil-mcp`에서 `reforge-mcp`로 변경됩니다. 기존 MCP 설정의 command 경로를 갱신해야 합니다. 릴리즈 아카이브도 새 이름을 사용합니다. 기존 로컬 데이터 재사용을 위해 `FOSSIL_CACHE_DIR`, `FOSSIL_LOG`와 SQLite 저장 경로는 유지합니다. GitHub 저장소는 `sjkim1127/reforge-mcp`로 변경했습니다. 기존 로컬 체크아웃 폴더는 그대로 사용해도 됩니다.

## 설계와 개발

- [제품 설계 및 단계별 완료 기준](docs/rust-porting-design.md)
- [기존 도구 계약과 운영 참고](docs/fossil-reference.md): 전환 전 문서이며 실행 파일 이름은 위 설정을 따릅니다.
- [릴리즈 절차](RELEASING.md)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
```

Apache-2.0 licensed. See [LICENSE](LICENSE).

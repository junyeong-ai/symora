<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/symora_black.png">
  <source media="(prefers-color-scheme: light)" srcset="assets/symora_white.png">
  <img alt="Symora" src="assets/symora_black.png" width="400">
</picture>

# Symora

**AI 코딩 에이전트를 위한 LSP 기반 코드 인텔리전스 CLI**

[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange?style=flat-square&logo=rust)](https://www.rust-lang.org)
[![DeepWiki](https://img.shields.io/badge/DeepWiki-junyeong--ai%2Fsymora-blue.svg?logo=data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAACwAAAAyCAYAAAAnWDnqAAAAAXNSR0IArs4c6QAAA05JREFUaEPtmUtyEzEQhtWTQyQLHNak2AB7ZnyXZMEjXMGeK/AIi+QuHrMnbChYY7MIh8g01fJoopFb0uhhEqqcbWTp06/uv1teleaEDv4O3n3dV60RfP947Mm9/SQc0teleIFQgzfc4CYZoTPAswgSJCCUJUnAAoRHOAUOcATwbmVLWdGoH//PB8mnKqScAhsD0kYP3j/Yt5LPQe2KvcXmGvRHcDnpxfL2zOYJ1mFwrryWTz0advv1Ut4CJgf5teleuhDuDj5eUcAUoahrdY/56teleebRWeraTjMt/00Sh3UDtjgHtQNHwcRGOC98teleJEAEymycmYcWwOprTgcB6VZ5JK5TAJ+fXGLBm3FDAmn6oPPjR4rKCAoJCal2eAiQp2x0vxTPB3ALO2CRkwmDy5WohzBDwSEFKRwPbknEggCPB/imwrycgxX2NzoMCHhPkDwqYMr9tRcP5qNrMZHkVnOjRMWwLCcr8ohBVb1OMjxLwGCvjTikrsBOiA6fNyCrm8V1rP93iVPpwaE+gO0SsWmPiXB+jikdf6SizrT5qKasx5j8ABbHpFTx+vFXp9EnYQmLx02h1QTTrl6eDqxLnGjporxl3NL3agEvXdT0WmEost648sQOYAeJS9Q7bfUVoMGnjo4AZdUMQku50McDcMWcBPvr0SzbTAFDfvJqwLzgxwATnCgnp4wDl6Aa+Ax283gghmj+vj7feE2KBBRMW3FzOpLOADl0Isb5587h/U4gGvkt5v60Z1VLG8BhYjbzRwyQZemwAd6cCR5/XFWLYZRIMpX39AR0tjaGGiGzLVyhse5C9RKC6ai42ppWPKiBagOvaYk8lO7DajerabOZP46Lby5wKjw1HCRx7p9sVMOWGzb/vA1hwiWc6jm3MvQDTogQkiqIhJV0nBQBTU+3okKCFDy9WwferkHjtxib7t3xIUQtHxnIwtx4mpg26/HfwVNVDb4oI9RHmx5WGelRVlrtiw43zboCLaxv46AZeB3IlTkwouebTr1y2NjSpHz68WNFjHvupy3q8TFn3Hos2IAk4Ju5dCo8B3wP7VPr/FGaKiG+T+v+TQqIrOqMTL1VdWV1DdmcbO8KXBz6esmYWYKPwDL5b5FA1a0hwapHiom0r/cKaoqr+27/XcrS5UwSMbQAAAABJRU5ErkJggg==)](https://deepwiki.com/junyeong-ai/symora)

[English](README.en.md) | **한국어**

---

## 이름의 의미

**Sym** (Symbol) + **ora** (라틴어: 경계, 문)

코드의 심볼 구조를 해독하고, 파일과 모듈의 경계를 넘어 관계의 문을 여는 분석 도구.

---

## 탄생 배경

[Serena](https://github.com/oraios/serena)에서 영감을 받았습니다.

| | Serena | Symora |
|---|--------|--------|
| 설계 철학 | 프레임워크 통합 | CLI 우선 |
| 인터페이스 | MCP 서버 | Bash 명령 |
| 언어 | Python | Rust |

설치 후 바로 `symora find refs src/main.rs:10:5` 실행 — Claude Code 스킬이나 셸 기반 AI 에이전트와 즉시 통합.

---

## 왜 Symora인가?

grep은 텍스트를 찾습니다. **Symora는 LSP를 통해 코드 구조를 분석합니다.**

```bash
# grep: 텍스트 패턴 매칭
grep -r "processOrder" .

# Symora: LSP 기반 코드 분석
symora find refs src/order.rs:42:5       # 이 심볼을 참조하는 모든 위치
symora find def src/api.rs:15:10         # 심볼 정의 위치
symora hover src/api.rs:15:10            # 타입 정보와 문서
symora calls incoming src/order.rs:42:5  # 호출 계층 분석
```

| 기능 | grep/ripgrep | Symora |
|------|--------------|--------|
| 정의로 이동 | ❌ | ✅ LSP |
| 참조 찾기 | ❌ | ✅ LSP |
| 타입 정보 | ❌ | ✅ LSP |
| 호출 계층 | ❌ | ✅ LSP |
| 리네임 리팩토링 | ❌ | ✅ LSP |
| 코드 검색 | ❌ | ✅ SQLite |
| AST 검색 | ❌ | ✅ tree-sitter |
| 사용량 메트릭 | ❌ | ✅ LSP |
| 문서 커버리지 | ❌ | ✅ LSP |
| 구조적 편집 | ❌ | ✅ tree-sitter |
| Git Diff 영향 분석 | ❌ | ✅ LSP |

---

## 지원 플랫폼

| 플랫폼 | 지원 | 비고 |
|--------|:----:|------|
| Linux (x86_64, aarch64) | ✅ | 전체 기능 |
| macOS (Apple Silicon) | ✅ | 전체 기능 |
| Windows | ❌ | Unix 소켓 의존성 |

> Symora는 데몬 IPC에 Unix 도메인 소켓을 사용합니다. Windows 지원은 현재 계획에 없습니다.

---

## 빠른 시작

```bash
cargo install --path .
symora doctor          # 언어 서버 확인
symora find symbol src/main.rs
```

---

## 핵심 기능

### LSP 기반 분석
```bash
symora find symbol src/main.rs --kind function   # 심볼 탐색
symora find def src/main.rs:10:5                 # 정의로 이동
symora find refs src/main.rs:10:5                # 참조 찾기
symora find refs src/main.rs:10:5 --with-snippet # 참조 + 소스 코드
symora find impl src/main.rs:10:5                # 구현체 찾기
symora hover src/main.rs:10:5                    # 타입/문서 정보
symora calls incoming src/main.rs:10:5           # 호출자 찾기
symora calls incoming src/main.rs:10:5 --no-fallback  # fallback 비활성화
symora rename src/main.rs:10:5 new_name          # 리네이밍
symora impact src/main.rs:10:5                   # 영향 분석
symora diagnostics src/main.rs                   # LSP 진단
symora diagnostics src/main.rs --with-context    # AI 친화적 컨텍스트 포함
symora diagnostics src/main.rs --with-suggestions # 수정 제안 포함
```

### 사용량 분석 (Usage Finder)
```bash
symora usage "process" --lang rust               # 패턴으로 심볼 검색
symora usage "Order" --lang rust --sort references     # 참조 수로 정렬
symora usage "Config" --lang rust --with-metrics # 상세 메트릭 포함
symora usage "*" --lang rust --filter no-docs    # 문서 미작성 심볼 찾기
symora usage "*" --lang rust --filter no-tests   # 테스트 미커버리지 심볼
symora usage "*" --lang rust --filter zero-refs  # 미사용 코드 탐지
symora usage "*" --lang rust --min-refs 5        # 중요 심볼 (5+ 참조)
symora usage "fn" --lang rust --with-snippet     # 코드 스니펫 포함
```

| 옵션 | 설명 |
|------|------|
| `--sort references\|name` | 정렬 기준 (기본: references) |
| `--filter` | has-tests, no-tests, has-docs, no-docs, not-test-file, zero-refs |
| `--with-metrics` | 참조 수, 테스트 유무, 문서 유무 표시 |
| `--with-snippet` | 소스 코드 스니펫 포함 |
| `--min-refs N` | 최소 참조 수 필터 (중요 심볼 찾기) |
| `--max-symbols N` | 분석할 최대 심볼 수 (기본: 50) |
| `--limit N` | 출력 결과 수 제한 (기본: 10) |

### 리팩토링
```bash
symora actions list src/main.rs:10:5              # 사용 가능한 코드 액션
symora actions list src/main.rs:10:5 --kind refactor  # 리팩토링만
symora actions apply src/main.rs:10:5 "Extract..."    # 액션 적용
```

### 컨텍스트 수집
```bash
symora context src/main.rs:10:5 --all                # 모든 컨텍스트 (호출자, 피호출자, 타입, 테스트)
symora context src/main.rs:10:5 --callers --callees  # 호출자/피호출자
symora context src/main.rs:10:5 --types --tests      # 타입 정의, 관련 테스트
```

### Git Diff 영향 분석
```bash
symora diff-impact                        # HEAD와 비교
symora diff-impact main                   # main 브랜치와 비교
symora diff-impact --staged               # 스테이징된 변경만 분석
symora diff-impact --callers              # 호출자 분석 포함
symora diff-impact --max-symbols 30       # 분석할 심볼 수 제한
```

| 출력 | 설명 |
|------|------|
| `changed_symbols_count` | 변경된 심볼 수 |
| `test_coverage.coverage_ratio` | 테스트 커버리지 비율 |
| `changes[].reference_count` | 심볼별 참조 수 |
| `changes[].callers` | 호출자 목록 (--callers 옵션) |

### 배치 처리
```bash
symora batch refs loc1 loc2 loc3                    # 여러 위치 일괄 조회
symora batch refs loc1 loc2 --with-snippet          # 스니펫 포함
symora batch refs loc1 loc2 --parallel --fail-fast  # 병렬 실행, 실패 시 중단
```

### 구조적 편집 (Pattern Edit)
```bash
symora edit pattern src/main.rs --pattern "(struct_item)" --lang rust --text "// NEW" --dry-run
symora edit replace src/main.rs:10:1 --text "new code" --dry-run
symora edit insert-after src/main.rs --symbol "MyFunc" --text "// comment" --dry-run
symora edit insert-before src/main.rs:10:5 --text "// comment" --dry-run
```

### 코드 검색
```bash
# 심볼/콘텐츠 검색 (SQLite LIKE 기반, 서브스트링 매칭 지원)
symora search symbols "execute" --kind function  # 심볼 검색
symora search symbols "Handler" --limit 10       # 결과 제한
symora search content "async fn" --lang rust     # 콘텐츠 검색
symora search content "TODO" --limit 20          # 결과 제한

# 검색 인덱스 관리
symora search index build                        # 인덱스 빌드
symora search index build --force --lang rust    # 강제 재빌드
symora search index status                       # 인덱스 상태
symora search index clear                        # 인덱스 삭제

# AST 검색 (tree-sitter)
symora search ast "function_item" --lang rust    # 구조적 검색
symora search nodes --lang rust                  # 노드 타입 조회
```

| 검색 타입 | 용도 | 엔진 |
|----------|------|------|
| `symbols` | 심볼 이름 검색 | SQLite |
| `content` | 코드 라인 검색 | SQLite |
| `ast` | 구조적 패턴 검색 | tree-sitter |

> **위치 형식**: `file:line:column` (1-indexed)
> **`--limit 0`**: 무제한 결과

---

## 지원 언어 (36개)

Rust, TypeScript, Python, Go, Java, Kotlin, C++, C#, Swift, Ruby, PHP, Haskell, TOML 등

```bash
symora doctor  # 설치된 언어 서버 확인
```

---

## 설정

```bash
symora config init           # 프로젝트 설정 (.symora/config.toml)
symora config init --global  # 글로벌 설정
```

주요 설정:
```toml
[lsp]
timeout_secs = 60       # LSP 타임아웃 (기본: 60초)
refs_limit = 500        # 참조 결과 제한
calls_limit = 100       # 호출 계층 제한
tests_limit = 10        # 테스트 결과 제한

[test]
file_patterns = ["_check.rs"]  # 커스텀 테스트 파일 패턴
dir_patterns = ["/verification/"]
markers = ["@MyTest"]
```

---

## 문제 해결

```bash
symora doctor           # 의존성 확인
symora daemon restart   # 데몬 재시작
symora daemon status    # 데몬 상태 확인
symora -v <command>     # 디버그 로깅 활성화
```

| 문제 | 해결 |
|------|------|
| LSP 타임아웃 | `symora daemon restart` |
| Kotlin 메서드 미반환 | `symora search ast "function_declaration" --lang kotlin` |
| Python 대규모 프로젝트 느림 | AST 검색 사용 또는 대기 |
| Usage 검색 느림 | `--max-symbols 30` 으로 분석 범위 축소 |
| Call hierarchy 미지원 | `--no-fallback` 없이 실행 (자동 refs fallback) |

---

## 링크

- [GitHub](https://github.com/junyeong-ai/symora)
- [개발자 가이드](CLAUDE.md)

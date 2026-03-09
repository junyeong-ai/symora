<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/symora_black.png">
  <source media="(prefers-color-scheme: light)" srcset="assets/symora_white.png">
  <img alt="Symora" src="assets/symora_black.png" width="400">
</picture>

# Symora

**AI 코딩 에이전트를 위한 심볼 중심 코드 인텔리전스 CLI**

[![Rust](https://img.shields.io/badge/rust-1.92%2B-orange?style=flat-square&logo=rust)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg?style=flat-square)](LICENSE)

[English](README.en.md) | **한국어**

---

## Symora란?

Symora는 AI 코딩 에이전트를 위해 설계된 CLI 우선 코드 인텔리전스 도구입니다.

다음을 결합합니다.

- LSP 기반 의미론 탐색
- SQLite 기반 심볼/콘텐츠 검색
- tree-sitter AST 검색
- 재사용 가능한 language server 세션을 위한 Unix daemon

Symora는 셸 기반 워크플로우, 구조화된 JSON 출력, 그리고 심볼이나 위치에서 시작하는 정확한 후속 분석에 맞춰 설계되었습니다.

---

## 왜 Symora인가?

텍스트 검색도 유용하지만, 에이전트는 보통 이런 질문에 답해야 합니다.

- 여기 있는 심볼은 무엇인가?
- 어디서 참조되는가?
- 누가 이것을 호출하는가?
- 다음에 어느 파일을 봐야 하는가?
- 변경 영향은 어디까지 퍼지는가?

Symora는 이런 흐름을 중심으로 만들어졌습니다.

```bash
# 대략적인 탐색
symora search symbols AuthUser

# 파일 단위 의미론 탐색
symora map file src/main.rs
symora symbols src/main.rs

# 위치에서 시작하는 정확한 후속 분석
symora context src/main.rs:42 --all
symora refs src/main.rs:42
symora usage src/main.rs:42:10
```

---

## 핵심 기능

### 의미론 탐색

```bash
symora symbols src/main.rs
symora def src/main.rs:10:5
symora refs src/main.rs:10:5
symora hover src/main.rs:10:5
symora callers src/main.rs:10:5
symora callees src/main.rs:10:5
symora typedef src/main.rs:10:5
symora impl src/main.rs:10:5
symora rename src/main.rs:10:5 new_name
```

### 검색과 탐색 시작점

```bash
symora search symbols AuthUser
symora search content "async fn"
symora search ast "(function_item)" --lang rust
symora search nodes --lang rust
```

### 프로젝트/파일 탐색

```bash
symora map summary
symora map file src/cli/commands/search.rs
symora map dir src/cli
symora map related src/cli/commands/search.rs
```

### 컨텍스트와 사용 분석

```bash
symora context src/main.rs:42 --all
symora refs src/main.rs:42
symora usage SearchCommand
symora usage src/cli/commands/search.rs:30:10
symora impact src/main.rs:42
symora diff-impact
```

### 편집 및 리팩터링 보조

```bash
symora actions list src/main.rs:42:5
symora actions apply src/main.rs:42:5 "Extract method"
symora edit replace src/main.rs:10:1 --text "new code" --dry-run
symora format src/main.rs
```

---

## 권장 워크플로우

Symora는 보통 아래 순서로 사용할 때 가장 잘 맞습니다.

1. `symora map summary` 로 프로젝트 진입점과 주요 영역 파악
2. `symora search symbols <query>` 로 workspace 단위 대략적 탐색
3. `symora map file <path>` 로 파일 개요 확인
4. `symora symbols <file>` 또는 `symora symbols --symbol <path>` 로 정확한 심볼 확인
5. `symora context`, `symora refs`, `symora usage` 로 정밀 후속 분석

이 역할 구분은 의도적입니다.

- `search symbols` 는 rough discovery 용도
- `symbols` 는 exact semantic inspection 용도
- `map file` 은 compact overview 용도이며 전체 심볼 덤프가 아닙니다

---

## 출력 모델

Symora는 기본적으로 JSON을 출력합니다.

주요 특징:

- 가능하면 프로젝트 상대 경로 사용
- `count`, `showing`, `truncated`, `hints` 같은 안정적인 리스트 필드 사용
- 토큰 절약을 위한 compact mode 제공

전역 옵션:

```bash
symora -c search symbols AuthUser   # compact JSON
symora -q refs src/main.rs:10:5     # 에러만 출력
symora -v status                    # 디버그 로그
```

---

## 검색 인덱스

Symora는 지속성 있는 SQLite 기반 검색 인덱스를 포함합니다.

```bash
symora search index build
symora search index build --force --lang rust
symora search index status
symora search index clear
```

인덱스는 현재 프로젝트의 `.symora/store.db`에 저장됩니다.

검색 명령은 인덱스나 의미론 기능이 약한 상황에서 fallback도 제공하지만, 반복 사용 기준으로는 인덱스를 유지하는 것이 가장 안정적입니다.

---

## 설정

설정 우선순위:

1. `.symora/config.toml`
2. `~/.config/symora/config.toml`
3. 기본값

설정 초기화:

```bash
symora config init
symora config init --global
```

주요 설정 항목:

- LSP timeout 및 limit
- daemon 동작
- 테스트 파일 패턴
- ignore 경로

---

## 플랫폼 및 런타임 참고사항

- Linux: 지원
- macOS: 지원
- Windows: daemon 기반 워크플로우는 지원하지 않음 (Unix domain socket 사용)

Unix 환경에서는 대부분의 명령이 기본적으로 daemon을 사용하고, 필요 시 direct LSP 실행으로 이어집니다.

Daemon 관련 명령:

```bash
symora daemon start
symora daemon stop
symora daemon restart
symora daemon status
```

---

## 설치

소스에서 설치:

```bash
cargo install --path .
```

환경 및 language server 확인:

```bash
symora doctor
```

---

## 실전 사용 메모

- `context`, `refs`, `usage` 는 `file:line:column` 형식 위치를 직접 받을 수 있습니다
- `usage` 는 위치를 주면 해당 심볼 이름을 자동으로 해석합니다
- `context` 는 active LSP 서버가 call hierarchy나 type definition을 잘 지원하지 않을 때 fallback guidance를 제공합니다
- `map related` 는 인접 파일을 찾는 heuristic helper이며, 완전한 dependency graph는 아닙니다

---

## 링크

- [개발자 가이드](CLAUDE.md)
- [GitHub 저장소](https://github.com/junyeong-ai/symora)

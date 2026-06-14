<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/symora_black.png">
  <source media="(prefers-color-scheme: light)" srcset="assets/symora_white.png">
  <img alt="Symora" src="assets/symora_black.png" width="400">
</picture>

# Symora

**AI 코딩 에이전트를 위한 심볼 중심 코드 인텔리전스 CLI**

[![Rust](https://img.shields.io/badge/rust-1.96%2B-orange?style=flat-square&logo=rust)](https://www.rust-lang.org)
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

- **의미론 탐색** — `symbols`, `def`, `refs`, `hover`, `callers`, `callees`, `typedef`, `implementations`
- **검색과 탐색 시작점** — `search symbols`, `search content`, `search ast`, 그리고 토큰 예산 기반 저장소 브리핑 `pack`
- **프로젝트/파일 탐색** — `map summary`, `map file`, `map dir`, `map related`
- **컨텍스트와 영향 분석** — `context`, `usage`, `impact`, `diff-impact`
- **편집 및 리팩터링** — `rename`, `actions`, `edit` 서브커맨드(심볼 또는 라인 지정 splice, 정확한 dry-run 미리보기, 참조 검증이 붙은 `delete`), `format`
- **상태 점검** — `diagnostics`, `doctor`, `status`

출력은 기본적으로 JSON이며(`pack`은 `--shape markdown`으로 붙여넣기용 플레인 텍스트도 지원), 각 명령의 `--help`에서 플래그와 옵션을 확인할 수 있습니다.

---

## AI 에이전트를 위해

에이전트용 플레이북 — 워크플로우 순서, 명령 선택, 출력 계약, 실패 처리 — 은 이 README가 아니라 도구와 함께 배포되어 바이너리와 항상 일치합니다.

- `symora setup skill` 은 Claude Code 스킬(전체 CLI 플레이북)을 설치합니다.
- `symora mcp serve` 는 동일한 가이드를 MCP `initialize` instructions로 반환합니다.

요약하면: 리스트 응답은 하나의 안정적인 형태(`count`, `showing`, `items`와 공개되는 `truncated`/`hints`/`next_commands`)를 공유하고, 위치는 1-indexed이며, 실패는 구조화된 `{code, message, hint}`로 전달되고, 탐색은 대략적(`pack`, `map summary`, `search symbols`)에서 정밀(`symbols`, `context`, `refs`, `impact`)로 흐릅니다. `--format compact`(단일 라인 JSON), `-q`(에러만 출력) 같은 전역 플래그는 서브커맨드 앞에 둘 수 있습니다.

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

일반 `search index build`는 변경된 파일만 다시 반영하고, 더 이상 존재하지 않는 파일은 정리합니다. `--force`는 전체 재구축이 필요할 때만 사용하면 됩니다.

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
- 언어 서버 실행 오버라이드 (`[lsp.servers.<lang>]`: command/args/tier)

```toml
[lsp.servers.typescript]
command = "/Users/me/.nvm/versions/node/v20.11.0/bin/typescript-language-server"
args = ["--stdio"]   # 생략 시 기본 args 상속
tier = "slow"        # 생략 가능; fast|standard|slow
```

키는 `symora doctor`가 출력하는 `language` id입니다 — 잘못된 키는 doctor의 `config_errors`로 보고되며 적용되지 않습니다. daemon은 시작 시 설정을 읽으므로 변경 후 `symora daemon restart`를 실행하세요.

---

## 플랫폼 및 런타임 참고사항

- Linux: 지원
- macOS: 지원
- Windows: daemon 기반 워크플로우는 지원하지 않음 (Unix domain socket 사용)

Unix에서는 기본적으로 daemon을 사용합니다(`SYMORA_NO_DAEMON=1`이면 in-process 직접 실행). 모드는 시작 시 한 번 결정되며 런타임 폴백은 없습니다. `daemon start`와 `daemon restart`는 백그라운드에서 daemon을 띄우고 바로 반환합니다.

Daemon 관련 명령:

```bash
symora daemon start
symora daemon stop
symora daemon restart
symora daemon status
```

---

## 설치

한 줄 설치 (최신 릴리스):

```bash
curl -fsSL https://raw.githubusercontent.com/junyeong-ai/symora/main/scripts/install.sh | bash
```

설치 스크립트는 검증된 바이너리를 배치하고(프롬프트에서 prebuilt/소스 빌드 선택), 원하면 Claude Code 스킬까지 설치합니다(`symora setup skill`에 위임). 나머지 셋업:

```bash
symora setup            # Claude Code 스킬 + 언어 서버 (대화형)
symora setup skill      # 스킬만
symora setup deps --group core   # 의존성만 (core / core-jvm / core-web / core-systems / all)
```

플래그/옵션 (curl-pipe와 함께 쓸 때):

```bash
# 특정 버전 핀
curl -fsSL https://raw.githubusercontent.com/junyeong-ai/symora/main/scripts/install.sh \
  | bash -s -- --version <version>

# GitHub build provenance 검증 (gh CLI 필요)
curl -fsSL https://raw.githubusercontent.com/junyeong-ai/symora/main/scripts/install.sh \
  | bash -s -- --verify-attestations

# 설치 위치 변경
curl -fsSL ... | SYMORA_INSTALL_DIR=/usr/local/bin bash

# 소스 빌드 + 스킬까지 무프롬프트 설치 (체크아웃 불필요 — 릴리스 태그를 git에서 직접 빌드)
curl -fsSL https://raw.githubusercontent.com/junyeong-ai/symora/main/scripts/install.sh \
  | bash -s -- --source --skill

# CI 등 비대화형: 프롬프트 없이 prebuilt + 스킬 스킵이 기본값
curl -fsSL ... | bash -s -- --prebuilt --no-skill
```

지원 타깃: macOS Apple Silicon, Linux x86_64 (gnu), Linux aarch64 (gnu). prebuilt가 없는 플랫폼(Intel Mac 등)은 같은 원샷 커맨드가 자동으로 소스 빌드로 진행합니다(Rust 필요, 체크아웃 불필요):

```bash
curl -fsSL https://raw.githubusercontent.com/junyeong-ai/symora/main/scripts/install.sh | bash
# 체크아웃 안에서는 ./scripts/install.sh --source 가 작업 트리를 빌드
```

업그레이드 / 제거 (스크립트 재실행 불필요 — 바이너리가 자기 lifecycle을 소유):

```bash
symora self update                    # 최신 릴리스로 in-place 교체
symora self update --version <version>    # 특정 버전 핀
symora self update --verify-attestations
symora self uninstall                 # 바이너리 + 스킬 + config + daemon 흔적 전부 제거
symora self uninstall --keep-skill --keep-config
```

환경 진단:

```bash
symora doctor          # 설치된 LSP / 누락된 LSP, 플랫폼별 설치 명령
```

---

## MCP 서버

Symora는 Model Context Protocol 서버로도 동작합니다. 주요 탐색·분석·편집 명령이 MCP 도구로 노출되며(전체가 아닌 선별된 집합), CLI와 동일한 in-process 명령 레이어를 공유하므로 두 표면의 결과는 일치합니다.

설치된 에이전트 호스트에 MCP 서버를 한 번에 연결합니다(idempotent, `--uninstall`로 역연결).

```bash
symora setup mcp                          # 감지된 호스트에 자동 연결 (Claude Code, Codex)
symora setup mcp --dry-run               # 변경 없이 적용 계획만 출력
symora setup mcp --host claude_code      # 특정 호스트만
symora setup mcp --uninstall             # 연결 해제 (자신이 만든 항목만 제거)
```

직접 실행하려면:

```bash
symora mcp serve                          # stdio (Claude Code, Cursor 등이 기본 사용)
symora mcp serve --transport http --port 8765
```

도구 목록과 입력 스키마는 `tools/list` 응답으로 확인할 수 있습니다. 소스 파일을 수정하는 도구는 두 곳에서 함께 표시되며(description의 `Mutates`, `annotations.readOnlyHint: false`) 모두 `dry_run`을 지원합니다. 서버의 `initialize` 응답에는 전체 사용 플레이북 — 도구 호출 순서, 편집 대상 지정, 오류 복구 — 이 포함되므로, 연결된 에이전트는 추가 설정이 필요 없습니다.

---

## 링크

- [개발자 가이드](CLAUDE.md)
- [GitHub 저장소](https://github.com/junyeong-ai/symora)

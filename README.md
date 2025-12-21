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
| 텍스트 검색 | ✅ | ✅ ripgrep |
| AST 검색 | ❌ | ✅ tree-sitter |

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
symora find impl src/main.rs:10:5                # 구현체 찾기
symora hover src/main.rs:10:5                    # 타입/문서 정보
symora calls incoming src/main.rs:10:5           # 호출자 찾기
symora rename src/main.rs:10:5 new_name          # 리네이밍
symora impact src/main.rs:10:5                   # 영향 분석
symora diagnostics src/main.rs                   # LSP 진단
```

### 코드 검색
```bash
symora search text "TODO" --type rust            # ripgrep 기반
symora search ast "function_item" --lang rust    # tree-sitter AST
symora search nodes --lang rust                  # 노드 타입 조회
```

> **위치 형식**: `file:line:column` (1-indexed)
> **`--limit 0`**: 무제한 결과

---

## 지원 언어 (36개)

Rust, TypeScript, Python, Go, Java, Kotlin, C++, C#, Swift, Ruby, PHP, Haskell 등

```bash
symora doctor  # 설치된 언어 서버 확인
```

---

## 설정

```bash
symora config init           # 프로젝트 설정 (.symora/config.toml)
symora config init --global  # 글로벌 설정
```

---

## 문제 해결

```bash
symora doctor           # 의존성 확인
symora daemon restart   # 데몬 재시작
symora daemon status    # 데몬 상태 확인
```

| 문제 | 해결 |
|------|------|
| LSP 타임아웃 | `symora daemon restart` |
| Kotlin 메서드 미반환 | `symora search ast "function_declaration" --lang kotlin` |
| Python 대규모 프로젝트 느림 | AST 검색 사용 또는 대기 |

---

## 링크

- [GitHub](https://github.com/junyeong-ai/symora)
- [개발자 가이드](CLAUDE.md)

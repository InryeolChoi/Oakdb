# 0012. 길이만 닿는 쓰기를 더한 정전 실험

이슈 #56의 검증 기록이다. 가짜 디스크(`SimFs`)의 정전에 "끝을 넘는 쓰기가 **길이만 닿고 내용은 0**"인 운명을 더했다(설계 0013). 그 뒤 세 가지를 확인했다. 엔진이 여전히 약속을 지키나, 예전에 못 잡던 변이 C3를 이제 잡나, 예전 시드의 뜻이 얼마나 바뀌었나.

- 환경: Apple M3 (Mac15,13), macOS 27.0.1. 정전 실험은 가짜 디스크라 OS와 무관하고, 회귀 테스트는 CI에서 macOS·Linux 둘 다 돈다
- 도구: `crates/oakdb/examples/crash_sim` (보고에 "길이만"·길이 변경·파일 이름을 찍고, `sweep`이 "길이만"이 나온 시드 수를 셈), `docs/experiments/scripts/mutants.py`

```sh
cargo run --release --example crash_sim -- sweep 0 100000
cargo run --release --example crash_sim -- every 7 8     # 42, 11, 23, 99도
python3 docs/experiments/scripts/mutants.py sim
python3 docs/experiments/scripts/mutants.py checkpoint
python3 docs/experiments/scripts/mutants.py wal
```

## 1. 정전 실험

| 결과 | 0010 (체크포인트 섞음) | 0012 (길이만 더함) |
|---|---|---|
| 시드 0~99,999 약속 지킴 | 100,000 / 100,000 | **100,000 / 100,000** |
| 그중 "길이만"이 하나라도 나온 시드 | — | 12,919 (12.9%) |

**모든 정전 시점** 훑기 (시점마다 정전 시드 8개):

| 작업 시드 | 커밋 | 정전 시점 | 하나라도 깨지는 시점 |
|---|---|---|---|
| 7 | 28 | 179 | **0** |
| 11 | 4 | 16 | **0** |
| 23 | 17 | 87 | **0** |
| 42 | 14 | 94 | **0** |
| 99 | 24 | 172 | **0** |

**엔진 버그는 나오지 않았다.** 새 고장이 엔진에 보여 주는 것은 세 자리다.

| 자리 | "길이만"이면 | 엔진이 버티는 이유 |
|---|---|---|
| 커밋을 WAL 끝에 덧붙임 | 마지막 커밋 자리가 0 | 0은 레코드 종류가 아니라 재생이 거기서 멈춘다. 응답하지 않은 커밋이라 잃어도 된다 |
| 체크포인트가 데이터 파일을 늘림 | 새 페이지가 0 | 아직 표지를 안 썼다. 열면 옛 표지 + WAL로 다시 옮긴다 |
| WAL 머리말 (비운 뒤) | 20바이트 0 | P1 수정: 20바이트 이하 WAL은 머리말이 없는 것으로 보고 새로 씀 |

### 변이를 심고 같은 실험 (10만 시드)

| 심은 것 | 깨진 시드 | 오류 |
|---|---|---|
| C16 (P1 수정 뺌: 20바이트 0 WAL 거절) | **811** (만드는 도중 150, 그 밖 661) | `WAL header is damaged` |
| C3 (비우기 뒤 sync 뺌) | **316** | `WAL header is damaged` |

새 고장 전에는 두 변이 모두 10만 시드에서 **0개**가 깨졌다(실험 0010 2절). 이제 정전 실험이 리뷰 P1을 스스로 찾는다.

C3가 깨지는 모양 (`crash_sim one 349`, C3를 심은 코드):

```text
power cut in the checkpoint after commit 9, or not at all
unsynced writes at the crash:
  oakdb.wal page 0: length only, zeros from byte 0
  oakdb.wal set to 0 bytes: lost
verdict: OpenFailed("data file is corrupt: WAL header is damaged")
```

비우기(`set_len(0)`)는 사라지고 머리말은 "길이만" 닿아, **옛 WAL의 앞 20바이트가 0**이 됐다. 뒤에 옛 레코드가 붙은 긴 WAL이라 "머리말이 깨진 WAL"로 거절된다. sync가 있으면 비우기가 먼저 굳어 있어서, 같은 "길이만"이 20바이트 0 WAL만 남긴다.

## 2. 변이 검사

| 모드 | 0010 | 0012 |
|---|---|---|
| `sim` (SimFs 버그) | 14/14 | **20/20** (이 고장 모델의 버그 6개를 더함) |
| `checkpoint` (C1~C19) | 18/19 (C3 놓침) | **19/19** |
| `wal` (W1~W11) | 11/11 | **11/11** |

새 SimFs 버그와 잡은 테스트:

| 심은 버그 | 잡은 테스트 (일부) |
|---|---|
| "길이만"이 안 나옴 | `a_write_past_the_end_can_land_as_its_length_alone_with_zeros` 외 3개 |
| "길이만"인데 내용을 씀 | 같은 4개 |
| "길이만"인데 파일이 안 늘어남 | 같은 4개 |
| 끝을 정전 때 디스크로 잼 | `the_end_that_counts_is_the_one_when_the_write_was_made`, `a_write_into_space_set_len_grew_is_inside_the_file` |
| 0을 쓰기 시작부터 (옛 끝이 아니라) | `only_the_part_past_the_old_end_reads_as_zeros` |
| 끝 안쪽 쓰기도 "길이만" | `a_crash_reports_what_became_of_each_unsynced_change` 외 3개 |

C3·C16을 잡는 엔진 테스트:

| 변이 | 잡은 테스트 |
|---|---|
| C3 | `a_power_cut_while_the_wal_starts_again_keeps_every_commit`, `a_cut_while_writing_the_wal_header_can_leave_its_length_and_zeros`, `emptying_the_wal_is_synced_before_its_header_is_written` (시드 349), `every_seed_keeps_the_promise` |
| C16 | 위 checkpoint 두 테스트, `a_wal_header_that_landed_as_zeros_opens` (시드 109), `every_seed_keeps_the_promise`, 기존 P1 테스트 3개 |

`mutants.py checkpoint`는 cargo가 처음 실패한 테스트 파일에서 멈추므로 `crash.rs`의 결과를 보이지 않는다. `crash.rs`만 따로 돌려 위 표를 확인했다.

## 3. 시드의 뜻이 바뀌었나 (완료 조건 5)

새 운명은 주사위를 더 던지지 않는다(설계 0013 3절). 그래서 **보고에 "길이만"이 없는 시드는 디스크가 예전과 똑같다.** 10만 시드 중 87.1%가 그렇다.

`tests/crash.rs`의 고정 시드:

| 시드 | 테스트 | 보고에 "길이만" | 판정 |
|---|---|---|---|
| 184 | 만드는 도중 정전 | 없음 | 예전과 같음 |
| 536 | 만드는 도중, 메타 뒤 | 없음 | 예전과 같음 |
| 213 | 새 페이지 없이 닿은 페이지 | 없음 | 예전과 같음 |
| 84 | 찢어진 잎 | 없음 | 예전과 같음 |
| 319 | 분할 도중 | 없음 | 예전과 같음 |
| 1680·406 | 잎 두 개 커밋 | 없음 | 예전과 같음 |
| 159 | 페이지 sync와 메타 사이 | 없음 | 예전과 같음 |
| 529 | 빈 목록 머리가 쓰는 중인 페이지 | **있음** (데이터 파일 page 14, 체크포인트 도중) | 바뀜. 여전히 약속 지킴 |

- 이 시드들의 주석은 **WAL 전 설계**가 깨지던 모습이다. WAL(#22)부터 연산 순서가 바뀌어 그 모습은 이미 재현되지 않는다. 지금은 "예전에 깨지던 시드가 계속 약속을 지키나"를 보는 테스트이고, 그 뜻은 그대로다.
- 529는 체크포인트가 데이터 파일 끝에 쓰던 페이지 하나가 "사라짐"에서 "길이만"으로 바뀌었다. 판정은 같다(`Panicked`가 아님, 실제로는 `Intact`).
- 새로 고정한 시드 2개(109: C16, 349: C3)는 "그 변이를 심으면 깨진다"가 뜻이다. 349는 맞는 코드에서는 다른 시점(커밋 5 도중)에서 끊긴다. C3가 sync를 하나 빼서 연산 수가 달라지기 때문이다. 주석에 적었다.
- 실험 0008~0010의 표를 지금 코드로 다시 돌리면 "길이만"이 나온 12.9%의 시드는 다른 디스크가 된다. 0010의 결과(100%)는 0012에서도 같았다.

## 4. 가설 판정

| 가설 (설계 때 적은 것) | 판정 | 근거 |
|---|---|---|
| 길이만 닿는 쓰기를 더하면 C3를 잡는다 | **채택** | `mutants.py checkpoint` 19/19, 정전 실험 316시드 |
| P1 수정을 빼면 정전 실험이 잡는다 | **채택** | 정전 실험 811시드, 고정 시드 109 |
| 지금 엔진은 이 고장에도 약속을 지킨다 | **채택** | 10만 시드, 모든 시점 훑기 5개, 새 체크포인트 테스트 |
| 주사위를 늘리지 않으면 대부분의 시드가 그대로다 | **채택** | 87.1%, 고정 시드 9개 중 8개 그대로 |

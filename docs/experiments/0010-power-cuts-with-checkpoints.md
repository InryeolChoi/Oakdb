# 0010. 체크포인트를 섞은 정전 실험과 재시작 시간

이슈 #23의 검증 기록이다. 실험 0009와 같은 도구에 "커밋 4개 중 하나 뒤에 체크포인트"를 섞어, 정전이 체크포인트 도중에도 일어나게 했다. 재시작 시간과 커밋 시간은 체크포인트 전후 엔진을 A/B로 비교했다.

- 환경: Apple M3 (Mac15,13), macOS 27.0.1, APFS, 내장 SSD. 정전 실험은 가짜 디스크(`SimFs`)라 OS와 무관하고, 회귀 테스트는 CI에서 macOS·Linux 둘 다 돈다
- 도구: `crates/oakdb/examples/crash_sim` (작업 목록에 체크포인트 추가), `docs/experiments/scripts/mutants.py checkpoint` (새 버그 목록), `ab_versions.py` (`reopen` 모드 추가)

```sh
cargo run --release --example crash_sim -- sweep 0 100000
cargo run --release --example crash_sim -- every 7 8
python3 docs/experiments/scripts/mutants.py checkpoint
python3 docs/experiments/scripts/ab_versions.py 58f8b8a <rev> 10 3000 reopen
python3 docs/experiments/scripts/ab_versions.py 58f8b8a <rev> 10 1500 grow
```

## 1. 정전 실험

체크포인트를 섞어도 시드 0의 작업은 같다. 체크포인트는 작업을 다 뽑은 **뒤에** 뽑아서, 커밋 목록이 0009와 같다. 정전 시점은 체크포인트의 연산만큼 늘었다.

| 결과 | 0009 (WAL만) | 0010 (체크포인트 섞음), 리뷰 전 | 리뷰 반영 뒤 |
|---|---|---|---|
| 시드 0~99,999 약속 지킴 | 100% | **100,000 / 100,000** | **100,000 / 100,000** |

**모든 정전 시점** 훑기 (시점마다 정전 시드 8개):

| 작업 시드 | 커밋 | 정전 시점 | 하나라도 깨지는 시점 |
|---|---|---|---|
| 7 | 28 | 179 | **0** |
| 11 | 4 | 16 | **0** |
| 23 | 17 | 87 | **0** |
| 42 | 14 | 94 | **0** |
| 99 | 24 | 172 | **0** |

`tests/checkpoint.rs`의 `a_power_cut_anywhere_in_a_checkpoint_keeps_every_commit`은 첫 정리(두 슬롯)와 세 번째 정리(한 슬롯)의 **모든 연산 뒤**에서 끊고, 시드 8개로 정전시킨 뒤 다시 커밋·정리·정전·열기까지 확인한다.

### 가설 판정

| 가설 (설계 때 적은 것) | 판정 | 근거 |
|---|---|---|
| 정리 도중 어디서 끊겨도 응답한 커밋은 남는다 | **채택** | 10만 시드, 모든 시점 훑기, 위 테스트 |
| 표지 한 장이 깨져도 다른 표지로 연다 (회차가 WAL과 맞을 때) | **채택** | `either_meta_slot_can_be_damaged_…`, 체크섬 테스트에서 메타 A·B의 모든 위치를 바꿔 봄 |
| 정리 뒤 최신 표지가 깨지면 거절해야 한다 | **채택** | 변이 C8을 `damage_to_the_newest_slot_once_its_wal_is_gone_is_refused`가 잡음 |

## 2. 변이 검사 (`mutants.py checkpoint`)

| 묶음 | 잡음 |
|---|---|
| 체크포인트 C1~C15 (리뷰 전) | 13/15 |
| 체크포인트 C1~C19 (리뷰 반영 뒤, 리뷰 결정마다 하나씩 더함) | **18/19** |
| WAL W1~W11 (코드가 옮겨 간 자리만 따라감) | 11/11 |

| 심은 버그 | 잡은 테스트 (일부) |
|---|---|
| C1·C2 표지 앞·뒤 sync 없음 | `a_power_cut_anywhere_in_a_checkpoint_keeps_every_commit` |
| C4 첫 정리가 한 슬롯만 | `a_new_database_records_the_page_size_in_both_meta_slots` |
| C5 최신 표지를 덮어씀 | `checkpoints_alternate_between_the_meta_slots` 외 2개 |
| C6 언제나 슬롯 A | 정전 훑기 테스트 |
| C7 옛 회차 WAL을 재생 | `a_wal_from_before_the_last_checkpoint_is_not_replayed` |
| C8 WAL이 표지보다 새것인데 엶 | `damage_to_the_newest_slot_once_its_wal_is_gone_is_refused` |
| C9 첫 정리 도중 정전을 복구 못 함 | 정전 훑기, `a_failed_checkpoint_stops_the_engine` |
| C10·C11 머리말 검사·사슬 시작 | `wal.rs` 단위 테스트들 |
| C12 실패해도 엔진이 안 멈춤 | `a_failed_checkpoint_stops_the_engine` |
| C13 커밋이 정리를 안 함 | `the_wal_stays_near_the_checkpoint_size_…` |
| C14 장부 길이 검사 없음 (리뷰 뒤 테스트 추가) | `a_data_file_shorter_than_its_meta_slot_says_is_refused` |
| C15 옮길 것 없어도 정리 | `a_checkpoint_with_nothing_to_move_changes_nothing` |
| C16 20바이트 WAL 머리말 거절 (P1) | `a_wal_of_one_header_length_whose_header_is_wrong_opens` 외 2개 |
| C17 첫 정리 복구가 아무 오류나 넘김 (P2) | `only_damage_lets_a_cut_first_checkpoint_…`, `choose_base` 단위 테스트 |
| C18 장부 길이 계산이 감김 (S3) | `a_meta_slot_with_more_pages_than_any_file_holds_is_refused` |
| C19 회차가 감김 (S3) | `a_checkpoint_number_that_cannot_grow_…` |

**놓친 하나: C3** (WAL 비우기와 머리말 쓰기 사이의 sync를 뺌). 이 sync는 "20바이트 이하 WAL에는 레코드가 없다"를 보장하고, P1 수정이 거기에 기댄다. SimFs는 찢어진 쓰기가 **파일 길이를 늘리지 않고**, 20바이트 쓰기는 한 섹터 안이라 통째로 닿거나 안 닿는다. 그래서 C3를 빼도 SimFs에서는 깨지는 경우가 생기지 않는다. 실제 파일 시스템(ext4 writeback, XFS)은 "길이는 닿고 내용은 0"을 남길 수 있다. P1도 리뷰가 코드를 읽고 찾았지, 실험이 찾지 못했다. 가짜 디스크의 고장 모델 밖이다.

## 3. 가설검정 (A/B)

A = 58f8b8a (체크포인트 전), B = cdc5e02. ABBA 순서 10라운드, Mann-Whitney U, 비율은 부트스트랩 95% CI.

| 가설 | 작업 | 중앙값 A → B | 비율 (95% CI) | p | 판정 |
|---|---|---|---|---|---|
| 다시 열기 시간이 다르다 | 커밋 3,000개 뒤 닫지 않고 다시 열기 | 18.7 → **8.6 ms** | 0.460 [0.324, 0.654] | 1.6e-4 | **채택, 54% 빨라짐** |
| `grow` 커밋 시간이 다르다 | 커밋 1,500개 (B는 정리 포함) | 2,854 → 3,020 µs | 1.058 [0.991, 1.175] | 0.096 | 기각 |
| A/A 대조군 | 같은 두 작업 | — | 0.997 · 1.023 | 0.94 · 0.15 | 측정 신뢰 가능 |

- 다시 열기: A는 WAL 약 13MB를 전부 재생하고, B는 4MiB 이하만 재생한다. 열기에는 장부 전체를 메모리로 읽는 시간도 들어 있다. 그래서 "재시작 시간 ∝ WAL 길이"는 **WAL 재생**에 대한 말이다 (설계 9절 P4).
- 커밋: 정리 비용은 커밋 수백 개에 나뉘어 평균에 드러나지 않았다. 정리를 맡은 커밋 하나의 지연 튐은 재지 않았다. 그룹 커밋 벤치마크(#30)에서 꼬리 지연과 함께 본다.
- 리뷰 반영(P1·P2·S3·정리)은 열기의 분기와 정리 앞의 검사만 바꿨다. 디스크 연산 수는 같아서 A/B를 다시 돌리지 않았다.

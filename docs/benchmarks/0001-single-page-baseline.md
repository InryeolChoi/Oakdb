# 0001. 벤치마크: 한 장짜리 저장의 기준선

이슈 #41. 01번 엔진(리프 한 장, 커밋마다 데이터 파일에 바로 쓰고 sync)의 속도를 처음으로 기록한다. 앞으로 B+Tree, WAL, 그룹 커밋을 더할 때마다 이 숫자와 비교한다.

## 무엇을 쟀나

`crates/oakdb/benches/single_page.rs` (criterion). 페이지를 4바이트 키·값 253개로 꽉 채운 상태에서 잰다.

| 벤치마크 | 하는 일 | 무엇을 보려고 |
|---|---|---|
| `get/hit` | 있는 키 하나 읽기 (트랜잭션 시작 포함) | 슬롯 이진 탐색 + 값 복사 비용 |
| `get/miss` | 없는 키 읽기 | 이진 탐색만의 비용 |
| `commit/no_sync` | 있는 키 하나 덮어쓰고 커밋, **sync 없음** | 엔진 자체 비용 (페이지 복사, 정리, 파일 쓰기) |
| `commit/durable` | 같은 일, **진짜 sync** | 사용자가 실제로 기다리는 시간 |

덮어쓰기라 페이지는 계속 꽉 차 있고, 거의 매번 페이지 정리(compact)가 일어난다. 그래서 `commit/no_sync`는 정리 비용까지 포함한 값이다.

## 환경

| 이름 | 기계 | OS · 파일시스템 | sync 방식 |
|---|---|---|---|
| 로컬 Mac | Apple M3 (Mac15,13), 내장 SSD | macOS 27.0.1, APFS | `F_FULLFSYNC` |
| CI Mac | GitHub Actions `macos-latest`, Apple M1 **가상 머신** 3코어 | macOS 26.6.2, APFS | `F_FULLFSYNC` |
| CI Linux | GitHub Actions `ubuntu-latest`, AMD EPYC 가상 머신 4코어, Azure 디스크 | Linux 6.17 (Azure), ext4 | `fdatasync` |

CI 숫자는 `.github/workflows/measure.yml`이 만든다. Actions 탭에서 손으로 다시 돌릴 수 있다.

## 결과 (중앙값, criterion 95% 구간의 가운데 값)

| 벤치마크 | 로컬 Mac | CI Mac (VM) | CI Linux (VM) |
|---|---|---|---|
| `get/hit` | 78 ns | 127 ns | 80 ns |
| `get/miss` | 31 ns | 50 ns | 62 ns |
| `commit/no_sync` | 2.9 µs | 3.4 µs | 3.4 µs |
| `commit/durable` | **3.0 ms** | 0.35 ms | 0.065 ms |
| → 초당 커밋 상한 (1 / durable) | **약 330** | 약 2,900 | 약 15,500 |

## 해석

1. **읽기는 100ns 안팎이다.** 키 253개를 이진 탐색하면 8번 비교다. 지금은 읽기마다 값을 `Vec`으로 복사하고, 트랜잭션 버퍼(`BTreeMap`)도 먼저 본다. B+Tree가 되어 페이지를 여러 장 타고 내려가면 이 숫자가 어떻게 늘어나는지 비교한다.
2. **엔진 자체의 커밋 비용은 약 3µs다.** 4KB 페이지를 복사하고, 정리하고, 파일에 쓰는 비용이다. 세 환경 모두 비슷하다.
3. **사용자가 기다리는 시간은 거의 전부 sync다.** 로컬 Mac에서 3ms 중 3µs만 엔진이고 나머지 99.9%가 디스크 기록이다. 실험 0002의 A/B 결과와 같다.
4. **CI Mac의 `F_FULLFSYNC`가 로컬보다 9배 빠르다.** 가상 머신의 가상 디스크는 "드라이브 캐시까지 비워라"를 진짜 드라이브까지 전달하지 않는 것으로 보인다. **CI Mac의 sync 숫자는 내구성 비용으로 믿으면 안 된다.** 엔진 자체 비용(`no_sync`, `get`)은 비교해도 된다.
5. **Linux `fdatasync`는 65µs다.** 클라우드 디스크라 로컬 SSD와 성격이 다르다. 로컬 Linux 기계에서의 값은 아직 없다.

## 한계

- 단일 스레드, 작은 키·값, 페이지 한 장. 실제 작업 부하(YCSB 등)는 벤치마크 티켓에서 다룬다.
- 같은 CI 환경도 실행마다 기계가 바뀐다(첫 실행 EPYC 7763, 두 번째 EPYC 9V74). 같은 실행 안에서의 비교만 믿는다.
- SQLite와의 비교는 아직 없다. 같은 조건(한 키 쓰기 + 커밋)을 맞춘 비교는 YCSB 단계에서 한다.

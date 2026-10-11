# 0005. B+Tree 페이지 구조 조사: 크기, 내부 배치, 큰 값, 체크섬

- 작성일: 2026-10-12
- 관련 이슈: #11 "B+Tree 페이지 구조" (#1의 하위 이슈)
- 목적: B+Tree 디스크 페이지의 네 가지 선택(페이지 크기, 페이지 안 배치, 페이지보다 큰 값, 페이지 체크섬)을 1차 출처로 확인하고, 이미 정한 결정(ADR 0006 커밋 직렬화, ADR 0007 페이지 전체 사본 redo WAL)에 맞는 권고를 낸다.
- 출발점(잠정 권고): 4KB 페이지, slotted page, 값 크기 상한(약 1/4 페이지)·v1에서 overflow 페이지 없음, 페이지마다 체크섬.
- 표기 규칙 (0001~0004와 같음)
  - **[사실]**: 링크한 1차 출처(공식 문서, 소스 코드, 논문, man page)에서 직접 확인한 내용
  - **[추론]**: 사실을 바탕으로 한 해석이나 계산
  - **[권고]**: 의견. 최종 결정은 ADR로 남긴다
  - **[미검증]**: 1차 출처로 직접 확인하지 못한 내용

---

## 한눈에 보기

**비유**: DB 파일은 두꺼운 **장부**이고, 페이지는 장부의 **한 장**이다. 장부는 언제나 한 장 단위로 꺼내 읽고 한 장 단위로 다시 끼운다. 이 문서는 네 가지를 정한다.

1. 한 장의 크기를 얼마로 할까 (페이지 크기)
2. 한 장 안에 항목을 어떻게 적을까 (페이지 안 배치)
3. 한 장에 안 들어가는 긴 항목은 어떻게 할까 (큰 값)
4. 한 장이 찢어지거나 번졌는지 어떻게 알아챌까 (체크섬)

결론부터 말하면 잠정 권고 네 가지는 모두 유지한다. 조사하면서 아래 내용이 더해졌다.

- 페이지 크기는 4KB로 하되, 코드에 상수로 박지 말고 **메타 페이지에 `page_size`를 기록**한다. ADR 0007은 커밋마다 바뀐 페이지 **전체**를 WAL에 쓴다. 그래서 페이지가 크면 WAL도 그만큼 커진다. 4KB를 고르는 가장 강한 근거가 이것이다.
- 값 상한은 "값 1/4 페이지"가 아니라 **셀 전체(키 + 값 + 셀 머리) ≤ 사용 가능 공간의 1/4**로 정한다. 키 상한도 따로 둔다. SQLite도 index b-tree에서 거의 같은 규칙(한 페이지에 최소 4개)을 쓴다.
- 체크섬은 **CRC-32C**로 하고, PostgreSQL처럼 **페이지 번호를 체크섬 계산에 섞는다**. 엉뚱한 위치에 써진 페이지도 잡을 수 있다.
- 페이지 머리에 **LSN 8바이트**를 둔다. ADR 0007의 "전체 사본 redo"만 쓴다면 지금 꼭 필요하지는 않다. 하지만 나중에 WAL을 줄이려고 PostgreSQL 방식으로 옮겨 가면 반드시 필요하다. 지금 넣는 비용은 8바이트뿐이다.

---

## 0. 용어 한 줄 정의

- **페이지(page)**: DB 파일을 같은 크기로 자른 한 조각. 읽기·쓰기·캐시의 단위.
- **섹터(sector)**: 디스크가 한 번에 읽고 쓰는 가장 작은 단위. 보통 512바이트나 4096바이트.
- **OS 페이지(VM page)**: 운영체제가 메모리를 관리하는 단위. DB 페이지와는 다른 개념이다.
- **fan-out**: 내부 노드 하나가 가리키는 자식 수. 클수록 트리가 낮아진다.
- **셀(cell)**: 페이지 안에 저장된 항목 하나(키 + 값, 또는 키 + 자식 페이지 번호).
- **슬롯(slot) / line pointer**: 셀이 페이지 안 어디에 있는지 적어 둔 작은 칸.
- **torn page(반쯤 쓴 페이지)**: 페이지를 쓰다가 전원이 나가서 일부는 새 내용, 일부는 옛 내용이 된 페이지.
- **체크섬(checksum)**: 내용으로 계산한 짧은 지문. 내용이 바뀌면 지문이 맞지 않는다.
- **LSN(log sequence number)**: WAL 레코드마다 붙는, 계속 커지는 번호(위치).
- **overflow 페이지**: 한 페이지에 다 못 넣은 긴 값의 나머지를 이어서 담는 별도 페이지.

---

## 1. 페이지 크기

**비유**: 장부 한 장이 크면 한 번 펼쳐서 많이 볼 수 있다(목차가 얕아진다). 대신 한 글자만 고쳐도 그 큰 장을 통째로 다시 써야 한다.

### 1.1 다른 DB의 기본값

- [사실] **SQLite**: 페이지 크기는 512~65536 사이의 2의 거듭제곱이다. 기본값은 오랫동안 1024였고, 3.12.0(2016-03-29)부터 4096이다. 문서는 "The default page size is recommended for most applications"라고 한다. 이미 만든 DB의 페이지 크기는 WAL 모드가 아닐 때 `VACUUM`을 해야 바뀐다 ([PRAGMA page_size](https://www.sqlite.org/pragma.html#pragma_page_size)).
- [사실] SQLite 파일 헤더의 offset 16에 페이지 크기(2바이트)가 들어 있고, 한 DB 안의 모든 페이지는 크기가 같다 ([File Format §1.2, §1.3.2](https://www.sqlite.org/fileformat2.html)).
- [사실] **PostgreSQL**: 테이블과 인덱스는 "usually 8 kB" 고정 크기 페이지의 배열이고, 컴파일할 때 바꿀 수 있다 ([Page Layout](https://www.postgresql.org/docs/current/storage-page-layout.html)). 빌드 스크립트의 `--with-blocksize` 기본값은 8(kB)이고 1, 2, 4, 8, 16, 32만 허용한다 ([configure.ac](https://github.com/postgres/postgres/blob/master/configure.ac)).
- [사실] **InnoDB(MySQL 8.4)**: `innodb_page_size` 기본값은 16KB다. 4KB, 8KB, 16KB, 32KB, 64KB 크기별 최대 테이블스페이스 크기가 표로 나와 있다. 페이지를 8KB나 4KB로 줄이면 최대 인덱스 키 길이도 비례해서 줄어든다(16KB에서 3072바이트, 4KB에서 768바이트) ([InnoDB Limits](https://dev.mysql.com/doc/refman/8.4/en/innodb-limits.html)).

### 1.2 OS 페이지 크기: macOS(Apple Silicon)와 Linux(x86)가 다르다

- [사실] Apple은 Apple silicon과 Intel Mac의 차이로 "Virtual memory page sizes are different"를 들고, 값을 `vm_page_size` 전역 변수에서 읽으라고 한다 ([Apple: Addressing architectural differences in your macOS code](https://developer.apple.com/documentation/apple-silicon/addressing-architectural-differences-in-your-macos-code)). 이 문서는 숫자를 적지 않는다.
- [사실, 직접 측정] 이 저장소를 개발하는 Apple silicon Mac(arm64)에서 `getconf PAGESIZE`, `sysctl hw.pagesize`, `sysctl vm.pagesize`가 모두 `16384`를 돌려줬다(2026-10-12).
- [사실] Linux 커널에서 x86은 `HAVE_PAGE_SIZE_4KB`만 선택하고, `PAGE_SIZE_4KB`는 "the standard 4KiB Linux page size"이며 `PAGE_SHIFT` 12다 ([arch/x86/Kconfig](https://github.com/torvalds/linux/blob/master/arch/x86/Kconfig), [arch/Kconfig](https://github.com/torvalds/linux/blob/master/arch/Kconfig)).
- [사실] Linux `getpagesize(2)`: "A user program should not hard-code a page size ... because some architectures support multiple page sizes" ([getpagesize(2) 소스](https://git.kernel.org/pub/scm/docs/man-pages/man-pages.git/plain/man/man2/getpagesize.2)).
- [추론] Oakdb는 `pread`/`pwrite` 계열(`Fs` trait의 `read_exact_at`/`write_all_at`)로 버퍼 I/O를 한다. 이때 DB 페이지가 OS 페이지와 같을 필요는 없다. 4KB 쓰기는 macOS에서 16KB 캐시 페이지의 일부를 고칠 뿐이다. OS 페이지 크기가 중요해지는 경우는 `mmap`으로 파일을 매핑하거나 direct I/O를 쓸 때다. 둘 다 v1 계획에 없다.

### 1.3 섹터와 원자적 쓰기: 페이지 쓰기는 "전부 아니면 전무"가 아니다

- [사실] PostgreSQL 문서: 디스크는 섹터 단위로 읽고 쓰며, PostgreSQL은 보통 8192바이트(512바이트 섹터 16개)를 한 번에 쓴다. 전원이 나가면 "some of the 512-byte sectors were written while others were not" ([WAL Reliability](https://www.postgresql.org/docs/current/wal-reliability.html)).
- [사실] SQLite는 기본 설정에서 섹터 쓰기도, 페이지 쓰기도 원자적이라고 가정하지 않는다. "SQLite never assumes atomic page writes in its default configurations." 유닉스·윈도 기본 VFS는 섹터 크기로 512를 보고해 왔다 ([PSOW](https://www.sqlite.org/psow.html), [Atomic Commit](https://www.sqlite.org/atomiccommit.html)).
- [사실] Linux NVMe 드라이버는 장치가 namespace 단위 원자 쓰기 크기(NAWUPF)를 알려 줄 때만 그 값을 쓴다. 알려 주지 않으면 원자 쓰기 단위를 **논리 블록 크기 하나**로 잡는다. 컨트롤러 전체 값(AWUPF)만 있으면 "AWUPF ignored, only NAWUPF accepted"를 출력한다 ([drivers/nvme/host/core.c](https://github.com/torvalds/linux/blob/master/drivers/nvme/host/core.c)).
- [사실] Linux 6.11부터 `pwritev2(..., RWF_ATOMIC)`로 "torn-write protection"을 요청할 수 있다. 길이는 2의 거듭제곱이어야 하고 `statx`가 알려 준 `[stx_atomic_write_unit_min, stx_atomic_write_unit_max]` 범위 안에 있어야 하며, 길이에 맞춰 정렬된 오프셋에 써야 한다. 일반 파일 지원은 xfs·ext4에서 Linux 6.13부터다 ([readv(2) 소스](https://git.kernel.org/pub/scm/docs/man-pages/man-pages.git/plain/man/man2/readv.2), [statx(2) 소스](https://git.kernel.org/pub/scm/docs/man-pages/man-pages.git/plain/man/man2/statx.2)).
- [사실] Linux `O_DIRECT`는 버퍼 주소, 길이, 파일 오프셋에 정렬 제약을 둘 수 있다. Linux 2.6부터는 보통 블록 장치의 논리 블록 크기(대개 512바이트) 배수다. `statx`의 `STATX_DIOALIGN`으로 조회할 수 있다 ([open(2) 소스](https://git.kernel.org/pub/scm/docs/man-pages/man-pages.git/plain/man/man2/open.2)).
- [추론] 정리하면 "4KB 페이지 쓰기가 원자적"이라는 보장은 macOS와 Linux 어느 쪽에서도 기본으로 주어지지 않는다. Linux에는 이를 요청하는 길이 새로 생겼지만 커널·파일시스템·장치 조건이 붙고 macOS에는 같은 기능이 없다(아래 [미검증] 참고). 그래서 ADR 0003(두 OS에서 같은 내구성 약속)에 따라 **원자적 페이지 쓰기에 기대지 않는다**. torn page는 체크섬으로 감지하고 WAL의 전체 사본으로 고친다(ADR 0007, 4절).

### 1.4 트레이드오프

**fan-out과 트리 높이** [추론, 계산]

가정: 페이지 머리 40바이트, 슬롯 4바이트, 키 16바이트, 자식 번호 8바이트, 리프 값 100바이트, 셀 머리(길이 등) 4~12바이트, 1천만 키.

| 페이지 | 내부 노드 fan-out | 리프당 키 | 리프 수 | 트리 높이 |
|---|---|---|---|---|
| 4KB | 약 135 | 약 30 | 약 33만 | 4 (내부 3 + 리프 1) |
| 16KB | 약 540 | 약 120 | 약 8.3만 | 3 (내부 2 + 리프 1) |

트리 높이 차이는 한 단계다. 내부 노드는 수가 적어 대부분 메모리 캐시에 머물기 때문에, 실제 디스크 읽기 차이는 이보다 작다.

**쓰기 증폭(write amplification)**: 실제로 바꾼 데이터에 비해 디스크에 쓰는 양이 몇 배인가.

- [추론] ADR 0007은 그룹 커밋 묶음마다 **바뀐 페이지 전체**를 WAL에 쓴다. 100바이트 값 하나를 고쳐도 4KB 페이지면 WAL에 약 4KB, 16KB 페이지면 약 16KB가 들어간다. 페이지 분할이 일어나면 페이지 두세 개가 더해진다. 이 설계에서는 페이지 크기가 곧 커밋 하나의 WAL 비용이다.
- [추론] 동시 쓰기(ADR 0005)를 벤치마크할 때 fsync 한 번에 실을 수 있는 커밋 수도 WAL 바이트에 따라 달라진다. 작은 페이지가 그룹 커밋에 유리하다.

**읽기 증폭**: 키 하나를 읽으려고 실제로 읽는 바이트 양. 작은 점 조회는 작은 페이지가, 긴 범위 스캔은 큰 페이지가 유리하다 [추론].

**정렬(alignment)**: 4KB는 Linux x86 OS 페이지, 흔한 4K 섹터 장치, `O_DIRECT` 정렬 조건(512 또는 4096의 배수)과 모두 맞는다. macOS 16KB VM 페이지와는 맞지 않지만, 버퍼 I/O에서는 정확성 문제가 없다 [추론].

### 1.5 권고

- [권고] **4096바이트로 시작한다.** SQLite 기본값과 같고, 페이지 전체 사본 WAL(ADR 0007)과 그룹 커밋에서 비용이 가장 작다.
- [권고] 페이지 크기는 상수 하나(`PAGE_SIZE`)로 모으고, **메타 페이지에 `page_size u32`를 기록**한다. 열 때 값이 다르면 거부한다. 0004 10.3의 메타 슬롯 스케치에는 이 필드가 없으므로 더한다. 나중에 8KB·16KB 벤치마크를 해 볼 길을 열어 둔다.
- [권고] OS 페이지 크기를 코드에 박지 않는다(getpagesize(2)의 권고). 버퍼 정렬이 필요하면 런타임에 조회한다.

---

## 2. 페이지 안 배치

**비유**: 장부 한 장의 **맨 위에는 목차**(몇 번째 항목이 어디 있는지)를 위에서 아래로 적고, **실제 항목은 맨 아래부터** 위로 채운다. 가운데 빈칸이 줄어들다가 두 쪽이 만나면 장이 꽉 찬 것이다. 항목을 정렬된 순서로 끼워 넣을 때는 목차만 한 칸씩 밀면 되고, 긴 항목 자체는 옮기지 않아도 된다.

### 2.1 PostgreSQL: line pointer + 끝에서부터 쌓는 tuple

- [사실] 페이지는 24바이트 `PageHeaderData`, 4바이트짜리 item identifier(line pointer) 배열, 빈 공간, item(실제 tuple), special space 순서다. "New item identifiers are allocated from the start of this area, new items from the end." item identifier는 (offset, length) 쌍이다 ([Page Layout](https://www.postgresql.org/docs/current/storage-page-layout.html)).
- [사실] 헤더 필드: `pd_lsn`(8B, 이 페이지를 마지막으로 바꾼 WAL 레코드의 끝 다음 바이트), `pd_checksum`(2B), `pd_flags`(2B), `pd_lower`(2B, 빈 공간 시작), `pd_upper`(2B, 빈 공간 끝), `pd_special`(2B, special space 시작), `pd_pagesize_version`(2B), `pd_prune_xid`(4B) ([Page Layout](https://www.postgresql.org/docs/current/storage-page-layout.html), [bufpage.h](https://github.com/postgres/postgres/blob/master/src/include/storage/bufpage.h)).
- [사실] special space는 access method마다 쓰는 영역이다. "b-tree indexes store links to the page's left and right siblings" ([Page Layout](https://www.postgresql.org/docs/current/storage-page-layout.html)).
- [사실] PostgreSQL B-tree는 Lehman & Yao 알고리즘을 구현한다. 페이지마다 오른쪽 형제를 가리키는 right-link와 high key를 더해서, 읽기 잠금 없이 검색하다가 동시 분할을 감지할 수 있게 한다 ([nbtree README](https://github.com/postgres/postgres/blob/master/src/backend/access/nbtree/README)).

### 2.2 SQLite: cell pointer array + cell content area + freeblock

- [사실] b-tree 페이지 머리는 리프 8바이트, 내부 노드 12바이트다. offset 0 페이지 종류(2, 5, 10, 13), offset 1 첫 freeblock 위치, offset 3 셀 개수, offset 5 셀 내용 영역 시작, offset 7 조각난 빈 바이트 수, offset 8 right-most pointer(4B, 내부 노드만) ([File Format §1.6](https://www.sqlite.org/fileformat2.html)).
- [사실] 머리 바로 뒤에 셀마다 2바이트 offset이 키 순서대로 놓인다(cell pointer array). 셀은 페이지 끝 쪽부터 채운다. 그 사이가 unallocated region이다 ([File Format §1.6](https://www.sqlite.org/fileformat2.html)).
- [사실] 셀을 지우면 생긴 빈자리는 **freeblock** 연결 리스트로 관리한다(freeblock 앞 4바이트 = 다음 freeblock 위치 + 크기). 4바이트보다 작은 1~3바이트 빈자리는 **fragment**로 세고, 정상 페이지에서 fragment 합은 60을 넘지 않는다. 전체 빈 공간 = unallocated + freeblock 합 + fragment 바이트 ([File Format §1.6](https://www.sqlite.org/fileformat2.html)).
- [사실] 파일 헤더 offset 20에 "reserved space" 바이트 수가 있다. 페이지마다 끝에 이만큼을 확장 기능용으로 남기며, 암호화 확장이 nonce나 암호학적 체크섬을 둔다. "usable size" = 페이지 크기 − reserved ([File Format §1.3.4](https://www.sqlite.org/fileformat2.html)).

### 2.3 고정 크기 배치(대안)

- [추론] 키와 값이 모두 고정 길이면 슬롯 없이 `i번째 항목 = 머리 + i × 크기`로 바로 계산할 수 있다. 단순하고 빠르지만, Oakdb는 키와 값이 가변 길이 바이트열(relational layer가 행을 직렬화해 넣는다)이라 맞지 않는다. 가장 긴 값 크기에 맞춰 칸을 잡으면 공간이 크게 낭비된다.

### 2.4 비교

| 항목 | PostgreSQL | SQLite | Oakdb 권고 |
|---|---|---|---|
| 슬롯 크기 | 4B (offset, length, flag) | 2B (offset만, 길이는 셀에서 해석) | **4B (offset u16, length u16)** |
| 지운 셀 공간 | 정리(prune/compaction) | freeblock 리스트 + fragment | **바이트 수만 세고, 모자라면 페이지를 통째로 다시 쓴다(compaction)** |
| 형제 링크 | special space에 좌·우 형제 | 없음 | **머리에 right_sibling** |
| 내부 노드의 n+1번째 자식 | (pivot tuple) | 머리의 right-most pointer | **머리의 rightmost_child** |
| LSN | `pd_lsn` 8B | 없음(WAL 프레임에 있음) | **page_lsn 8B** |
| 체크섬 | `pd_checksum` 2B | 없음(cksumvfs는 끝 8B) | **4B CRC-32C** |

- [권고] **슬롯에 길이를 함께 둔다.** 셀을 해석하지 않고도 경계 검사를 할 수 있어서, 깨진 페이지를 읽을 때 엉뚱한 곳을 읽는 실수를 줄인다. 4KB 페이지에서는 u16 offset이면 충분하다.
- [권고] **freeblock 리스트는 v1에서 만들지 않는다.** 대신 셀을 지우거나 줄일 때 생긴 빈 바이트 합(`frag_bytes`)만 기록한다. 삽입할 셀이 가운데 빈 공간에 안 들어가지만 `가운데 빈 공간 + frag_bytes`에는 들어가면, 페이지를 메모리에서 새로 압축해 쓴다. [추론] ADR 0007은 바뀐 페이지를 통째로 WAL에 쓰므로, 압축은 메모리 안의 일일 뿐이고 로그 형식에 영향이 없다. freeblock 리스트는 공간을 아끼는 최적화이고, 정확성 테스트 대상만 늘린다.
- [권고] **right_sibling을 리프에 둔다.** 범위 스캔(relational layer의 테이블 스캔)이 부모로 올라가지 않고 옆 리프로 갈 수 있다. ADR 0006이 나중에 래치 크래빙이나 B-link로 갈 때도 Lehman & Yao의 right-link 자리가 이미 있다. v1에서는 왼쪽 형제는 두지 않는다(병합 때만 필요하다).
- [권고] 모든 정수는 **little-endian으로 고정**한다. [사실] PostgreSQL은 `pd_lsn`의 저장 방식이 "for historical reasons" CPU endianness에 달려 있어서 읽고 쓸 때 변환해야 한다고 적는다 ([bufpage.h](https://github.com/postgres/postgres/blob/master/src/include/storage/bufpage.h)). 처음부터 고정해 두면 이런 짐이 없다.

### 2.5 머리 필드 제안 (4096B 페이지, 40B 머리)

```
offset  크기  필드              설명
0..4    u32   checksum          CRC-32C. 계산 범위: 4..4096 + page_no (4절)
4..12   u64   page_lsn          이 페이지를 마지막으로 바꾼 커밋 묶음의 LSN (5절)
12      u8    kind              1 = leaf, 2 = internal, 3 = free (0은 오류)
13      u8    flags             v1은 0. 알 수 없는 비트가 있으면 오류
14..16  u16   cell_count        셀 개수 = 슬롯 개수
16..18  u16   free_start        슬롯 배열 끝 (PG pd_lower)
18..20  u16   free_end          셀 영역 시작 (PG pd_upper, SQLite offset 5)
20..22  u16   frag_bytes        지운 셀이 남긴 빈 바이트 합 (SQLite fragment + freeblock 역할)
22..24  u16   reserved          0
24..32  u64   right_sibling     leaf: 오른쪽 leaf 페이지 번호, 없으면 0
32..40  u64   rightmost_child   internal: 마지막 키보다 큰 쪽 자식. leaf는 0
40..    [u16 offset, u16 len] × cell_count   슬롯 배열 (키 순서)
...     빈 공간
...4096 셀들 (끝에서부터)
```

- 0004 10.3 스케치(`crc32c`, `page_lsn`, `kind` 순서)를 그대로 잇고 나머지를 채웠다.
- [추론] 페이지 번호에 u64를 쓰면 0004 메타 슬롯(`root_page u64`, `page_count u64`)과 맞는다. u32로 줄이면 내부 노드 셀이 4바이트 짧아져 fan-out이 10% 남짓 커지지만, 4KB × 2³² = 16TB 한계가 생긴다. v1에서는 단순함을 택해 u64로 둔다.
- [권고] 페이지 번호 0, 1은 메타 슬롯 A/B이므로 `right_sibling = 0`을 "없음"으로 써도 겹치지 않는다.
- [권고] 셀 형식: leaf = `key_len u16 | val_len u16 | (MVCC 메타, 별도 이슈) | key | value`, internal = `key_len u16 | child u64 | key`. MVCC 버전을 트리에 어떻게 둘지는 이 문서 범위 밖이다.
- [권고] 읽을 때 머리를 먼저 검증한다: `40 ≤ free_start ≤ free_end ≤ 4096`, `free_start = 40 + 4 × cell_count`, 모든 슬롯이 `[free_end, 4096)` 안. 체크섬이 맞아도 버그로 만든 페이지를 잡을 수 있다. 속성 기반 테스트(proptest)에 그대로 쓸 수 있다.

---

## 3. 페이지보다 큰 값

**비유**: 장부 한 장에 안 들어가는 긴 메모를 어떻게 할까. (a) 앞부분만 적고 "나머지는 부록 37쪽"이라고 적는다(overflow). (b) 메모를 별도 서류철에 넣고 장부에는 서류철 번호만 적는다(TOAST). (c) "한 항목은 이 길이까지만"이라고 규칙을 정한다(상한).

### 3.1 SQLite: overflow 페이지 체인

- [사실] usable size를 U, payload 크기를 P라 하자. **table b-tree 리프**는 X = U − 35 이하면 전부 페이지 안에 둔다. **index b-tree**는 X = ((U − 12) × 64 / 255) − 23 이하일 때만 전부 둔다. 넘으면 앞부분 K(또는 최소 M = ((U − 12) × 32 / 255) − 23)바이트만 페이지에 두고 나머지는 overflow 페이지로 보낸다 ([File Format §1.6](https://www.sqlite.org/fileformat2.html)).
- [사실] overflow 페이지는 첫 4바이트가 다음 overflow 페이지 번호(마지막이면 0)이고 나머지가 내용이다 ([File Format §1.7](https://www.sqlite.org/fileformat2.html)).
- [추론, 계산] U = 4096이면 index b-tree의 X = (4084 × 64 / 255) − 23 = 1002, M = 489다. 즉 SQLite도 **키가 곧 내용인 index b-tree에서는 셀 하나를 약 1/4 페이지로 제한**한다. 한 페이지에 셀이 최소 4개 들어가야 분할이 제대로 되기 때문이다. Oakdb는 키와 값을 함께 저장하는 B+Tree이므로 SQLite index b-tree와 처지가 같다.

### 3.2 PostgreSQL: TOAST

- [사실] "PostgreSQL uses a fixed page size (commonly 8 kB), and does not allow tuples to span multiple pages." 그래서 큰 필드는 압축하거나 여러 행으로 쪼갠다. 행이 `TOAST_TUPLE_THRESHOLD`(보통 2kB)보다 크면, `TOAST_TUPLE_TARGET`(보통 2kB)보다 작아질 때까지 필드를 압축하거나 행 밖으로 옮긴다. 밖으로 옮긴 값은 약 2000바이트 chunk로 나뉘어 TOAST 테이블에 (chunk_id, chunk_seq, chunk_data) 행으로 들어간다. 값 하나의 최대 크기는 1GB다 ([TOAST](https://www.postgresql.org/docs/current/storage-toast.html)).
- [추론] 8kB 페이지에서 2kB 문턱은 역시 "1/4 페이지"다.

### 3.3 InnoDB: off-page 컬럼

- [사실] 최대 행 크기는 off-page 컬럼을 빼고 4·8·16·32KB 페이지에서 "slightly less than half of a page"다(16KB에서 약 8000바이트). 행이 반 페이지를 넘으면 가변 길이 컬럼을 off-page로 보내 반 페이지 안에 들어가게 한다 ([InnoDB Limits](https://dev.mysql.com/doc/refman/8.4/en/innodb-limits.html)).
- [사실] DYNAMIC 행 형식은 긴 컬럼을 통째로 off-page에 두고 레코드에는 20바이트 포인터만 남긴다. COMPACT는 앞 768바이트를 레코드에 남긴다. off-page 값은 컬럼마다 overflow 페이지 단일 연결 리스트에 들어간다 ([InnoDB Row Formats](https://dev.mysql.com/doc/refman/8.4/en/innodb-row-format.html)).

### 3.4 Oakdb v1에 상한을 두면

- [추론] 세 DB 모두 "한 페이지에 최소 2~4개"를 보장하는 문턱을 두고, 그 위만 별도 저장소로 보낸다. Oakdb v1은 문턱까지만 지원하고 별도 저장소를 만들지 않는다. 범위가 크게 줄어든다.
  - 줄어드는 것: overflow 체인 할당·해제, 값을 읽을 때 여러 페이지 따라가기, 커밋 묶음의 WAL에 overflow 페이지까지 넣기, 빈 페이지 회수.
  - 대가: 행 하나(relational layer가 직렬화한 값)가 약 1KB를 넘으면 오류다. 긴 TEXT·JSON 컬럼을 받을 수 없다. 사용자 문서와 오류 메시지에 이 한계를 적어야 한다.
- [권고] 상한을 **셀 전체 기준**으로 정의한다: `셀 머리 + 키 + 값 ≤ (4096 − 40) / 4 − 4`(슬롯 4바이트) = **1010바이트**. 상수 하나로 두고 테스트로 "어떤 셀 4개도 빈 페이지에 들어간다"를 확인한다.
- [권고] **키 상한을 따로 둔다**(예: 512바이트). 내부 노드에는 키가 구분자로 올라가므로, 키가 길면 fan-out이 떨어진다. 값은 짧고 키가 긴 경우를 막는다.
- [권고] 상한을 넘으면 `put`이 명확한 오류(`ValueTooLarge { max }`)를 돌려준다. 잘라 넣지 않는다.
- [권고] 셀 형식의 `val_len`을 u16으로 두되, 나중에 overflow를 붙일 수 있게 `flags`에 "값이 밖에 있음" 비트 자리를 남겨 둔다(v1은 항상 0).

---

## 4. 페이지 체크섬

**비유**: 장부 한 장마다 맨 위에 "이 장 글자 수 합계"를 적어 둔다. 다시 펼쳤을 때 합계가 안 맞으면 누가 지웠거나 물에 번진 것이다. 합계는 **잘못됐다는 것만** 알려 준다. 원래 내용은 다른 곳(WAL의 사본)에서 가져와야 한다.

### 4.1 다른 DB

- [사실] **PostgreSQL**: 데이터 페이지만 체크섬으로 보호한다. 페이지를 쓸 때 갱신하고, 읽을 때마다 검사한다. 클러스터 단위로만 켜고 끈다 ([Data Checksums](https://www.postgresql.org/docs/current/checksums.html)). PostgreSQL 18부터 `initdb`가 **기본으로 체크섬을 켠다**. 끄려면 `--no-data-checksums`를 쓴다 ([PG 18 Release Notes](https://www.postgresql.org/docs/18/release-18.html), [initdb](https://www.postgresql.org/docs/current/app-initdb.html)).
- [사실] PostgreSQL 페이지 체크섬은 FNV-1a를 바탕으로 한 알고리즘이다. 계산할 때 체크섬 필드를 잠시 0으로 두고, **블록 번호를 섞는다**("to detect the case where a page is somehow moved to a different location"). 결과는 16비트로 줄인다 ([checksum_impl.h](https://github.com/postgres/postgres/blob/master/src/include/storage/checksum_impl.h)).
- [사실] PostgreSQL WAL 레코드는 CRC-32C(32비트)로 보호한다. WAL 안의 full page image도 항상 체크섬으로 보호된다 ([WAL Reliability](https://www.postgresql.org/docs/current/wal-reliability.html)).
- [사실] 체크섬을 켜면 hint bit만 바뀐 경우도 항상 WAL에 기록한다(`wal_log_hints` 무시) ([WAL 설정](https://www.postgresql.org/docs/current/runtime-config-wal.html)). [추론] 로그 없이 페이지를 고치면 그 페이지가 찢어졌을 때 체크섬만 깨지고 고칠 사본이 없기 때문이다. Oakdb 교훈: **WAL을 거치지 않고 페이지를 고쳐 쓰는 경로를 만들지 않는다.**
- [사실] **SQLite**: 코어에는 페이지 체크섬이 없다. 확장 `cksumvfs`가 페이지 끝 reserved 8바이트에 체크섬을 넣는다. 쓸 때 계산하고 읽을 때 검사하며, 틀리면 `SQLITE_IOERR_DATA`를 돌려준다. reserve bytes 기본값이 0이라 새 DB에는 체크섬이 없다 ([cksumvfs](https://www.sqlite.org/cksumvfs.html)). 알고리즘은 32비트 워드 두 개를 누적하는 Fletcher 계열 합이다 ([cksumvfs.c](https://github.com/sqlite/sqlite/blob/master/ext/misc/cksumvfs.c)).
- [사실] **InnoDB**: `innodb_checksum_algorithm` 기본값은 `crc32`다(`crc32`, `strict_crc32`, `innodb`, `strict_innodb`, `none`, `strict_none`) ([InnoDB Startup Options](https://dev.mysql.com/doc/refman/8.4/en/innodb-parameters.html)). 소스 주석에 따르면 이 CRC32는 SSE 4.2 명령에 맞춰 다항식을 바꿨고(`0x82f63b78`, 0x1EDC6F41의 비트 반전), zlib CRC32와 값이 다르다 ([ut/crc32.cc](https://github.com/mysql/mysql-server/blob/8.4/storage/innobase/ut/crc32.cc)). [추론] 즉 InnoDB의 "crc32"는 CRC-32C(Castagnoli)다.
- [사실] InnoDB는 torn page를 doublewrite buffer로 고친다. 페이지를 제자리에 쓰기 전에 doublewrite 영역에 먼저 써서, 쓰다가 멈추면 복구 때 "a good copy of the page"를 찾는다. 기본값은 `ON` ([Doublewrite Buffer](https://dev.mysql.com/doc/refman/8.4/en/innodb-doublewrite-buffer.html), [innodb_doublewrite](https://dev.mysql.com/doc/refman/8.4/en/innodb-parameters.html)).

### 4.2 알고리즘: CRC-32C

- [사실] CRC-32C(Castagnoli, 생성 다항식 0x11EDC6F41)는 iSCSI 표준에 정의돼 있다 ([RFC 3720](https://www.rfc-editor.org/rfc/rfc3720)).
- [사실] Rust 표준 라이브러리에 x86-64 SSE4.2(`_mm_crc32_u64`)와 aarch64(`__crc32cd`) CRC-32C intrinsic이 있다 ([core::arch::x86_64::_mm_crc32_u64](https://doc.rust-lang.org/core/arch/x86_64/fn._mm_crc32_u64.html), [core::arch::aarch64::__crc32cd](https://doc.rust-lang.org/core/arch/aarch64/fn.__crc32cd.html)). `crc32c` crate(0.6.8)는 x86-64 SSE 4.2와 aarch64 crc 기능으로 하드웨어 가속을 하고, 없으면 소프트웨어로 계산한다 ([crc32c README](https://github.com/zowens/crc32c)).
- [추론] 두 개발·CI 환경(Apple silicon, Linux x86-64) 모두 하드웨어 명령이 있다. WAL 레코드도 이미 CRC-32C로 정했으므로(0004 10.4) 알고리즘을 하나로 맞추면 코드와 테스트가 하나로 줄어든다.

### 4.3 torn page, WAL 전체 사본과의 관계

- [사실] PostgreSQL은 체크포인트 뒤 각 페이지의 첫 변경 때 페이지 전체를 WAL에 쓴다. 반쯤 쓴 페이지는 "a mix of old and new data"라서 행 단위 WAL만으로는 고칠 수 없기 때문이다 ([WAL 설정, full_page_writes](https://www.postgresql.org/docs/current/runtime-config-wal.html)).
- [추론] Oakdb(ADR 0007)에서 각자의 역할:
  1. **WAL PAGE 레코드의 CRC**: WAL 안의 사본이 온전한지 확인한다. 깨진 첫 레코드에서 재생을 멈춘다.
  2. **데이터 파일 페이지의 체크섬**: 체크포인트 때 데이터 파일에 쓰다 찢어진 페이지, 또는 시간이 지나 상한 페이지를 찾는다.
  3. **고치는 방법**: 데이터 파일 페이지는 체크포인트 때만 쓰고, 그 체크포인트가 끝나 데이터 파일을 fsync하기 전에는 이전 체크포인트 이후의 WAL을 지우지 않는다. 그래서 체크포인트 중 찢어진 페이지는 항상 WAL에 온전한 사본이 있다. 재시작 때 WAL을 다시 적용하면 덮어써진다.
  4. **고칠 수 없는 경우**: 체크포인트가 끝난 뒤 생긴 비트 손상(bit rot)은 WAL에 사본이 없다. 이때는 체크섬 오류를 돌려주고 멈춘다. 체크섬은 **감지**만 하고 **복구**는 하지 않는다.
- [권고] 체크섬 계산 범위: 페이지의 4..4096 바이트 + **페이지 번호(u64, little-endian)**. PostgreSQL처럼 엉뚱한 위치에 써진 페이지(misdirected write)도 잡는다. 페이지 번호는 머리에 따로 저장하지 않아도 된다(읽을 때 이미 안다).
- [권고] 체크섬은 **데이터 파일에 쓸 때** 계산하고, **디스크에서 읽을 때마다** 검사한다. 메모리 캐시에 있는 동안 고칠 때마다 다시 계산할 필요는 없다. WAL 사본은 WAL 레코드 CRC로 보호되므로 이중으로 계산하지 않아도 되지만, 복구 때 WAL 사본을 그대로 데이터 파일에 쓰려면 사본에도 체크섬이 맞아 있어야 한다. 그래서 **WAL에 이미지를 복사할 때 페이지 체크섬도 함께 계산**하는 쪽이 단순하다.
- [권고] 모두 0인 페이지(파일을 늘린 직후)는 체크섬 0을 "아직 안 쓴 페이지"로 볼지 정해야 한다. v1에서는 **체크섬이 틀리면 무조건 오류**로 하고, 파일을 늘릴 때 빈 페이지를 올바른 체크섬과 함께 쓴다. 예외 규칙을 두지 않는다.

---

## 5. LSN을 페이지에 두는 이유 (짧게)

**비유**: 장부 한 장 귀퉁이에 "이 장은 메모장 몇 번 항목까지 반영됨"이라고 적어 둔다. 메모장(WAL)을 다시 읽으며 복구할 때, 이미 반영된 항목은 건너뛰고 아직 안 된 것만 적용할 수 있다.

- [사실] PostgreSQL: WAL의 기본 가정은 "log entries must reach stable storage before the data-page changes they describe"다. 이를 위해 데이터 페이지마다 마지막으로 영향을 준 WAL 레코드의 LSN을 표시하고, 버퍼 관리자는 더러운 페이지를 쓰기 전에 WAL을 최소한 그 LSN까지 디스크에 내린다. 재생할 때 페이지 LSN이 로그 레코드 위치 이상이면 그 변경은 이미 적용된 것이다 ([transam README](https://github.com/postgres/postgres/blob/master/src/backend/access/transam/README)). `bufpage.h`는 이를 "thou shalt write xlog before data"라고 부른다 ([bufpage.h](https://github.com/postgres/postgres/blob/master/src/include/storage/bufpage.h)).
- [사실] ARIES도 페이지마다 LSN을 두어 페이지 상태와 로그를 연결하고, redo 때 페이지 LSN을 보고 이미 반영된 변경을 건너뛴다(0004 4절 정리, [ARIES 초록](https://www.cs.brandeis.edu/~liuba/abstracts/mohan.html), Mohan et al., ACM TODS 17(1), 1992).
- [추론] ADR 0007(커밋마다 페이지 전체 사본, 데이터 파일은 체크포인트 때만)에서는:
  - redo가 "사본으로 덮어쓰기"라서 여러 번 해도 결과가 같다. 페이지 LSN을 보고 건너뛰지 않아도 정확하다.
  - 체크포인트는 이미 fsync된 WAL 안의 이미지만 데이터 파일에 쓰므로, "WAL 먼저" 규칙이 구조상 자동으로 지켜진다.
  - 그래도 LSN을 두는 이유: (1) ADR 0007 Consequences가 말한 대로 나중에 "체크포인트 뒤 첫 변경만 전체 사본, 나머지는 작은 레코드"로 바꾸면, 작은 레코드는 덮어쓰기가 아니어서 페이지 LSN 비교가 꼭 필요하다. (2) 체크포인트 때 "이 페이지의 LSN ≤ fsync된 WAL 끝"을 assert로 검사하면 순서 버그를 테스트에서 잡는다. (3) 장애 실험(docs/experiments)에서 페이지가 어느 커밋 묶음 상태인지 바로 보인다.
- [권고] 머리 offset 4에 `page_lsn u64`를 둔다(PostgreSQL처럼 머리 맨 앞쪽). 값은 그 페이지를 바꾼 커밋 묶음의 COMMIT 레코드 LSN(또는 그 PAGE 레코드 LSN) 중 하나로 정하고 ADR에 적는다. 0004 스케치는 "커밋 묶음의 LSN"이다.

---

## 6. 권고 요약

| 선택 | 잠정 권고 | 최종 권고 | 바뀐 점 / 근거 |
|---|---|---|---|
| 페이지 크기 | 4KB | **4096B**, 메타 페이지에 `page_size u32` 기록, 열 때 불일치하면 거부 | 그대로. 근거를 더함: 페이지 전체 사본 WAL(ADR 0007)에서는 페이지 크기 = 커밋당 WAL 비용. SQLite 기본값. 메타 필드는 새로 추가 |
| 페이지 안 배치 | slotted page | **slotted page**: 슬롯 4B(offset u16 + len u16), 셀은 끝에서부터, freeblock 리스트 없이 `frag_bytes` + 압축, 리프에 `right_sibling`, 내부에 `rightmost_child`, little-endian 고정 | 그대로. 구체화: freeblock 대신 압축(ADR 0007 덕분에 로그 영향 없음), 슬롯에 길이 포함 |
| 큰 값 | 값 ≤ 약 1/4 페이지, overflow 없음 | **셀 전체(머리 + 키 + 값) ≤ 1010B**, **키 ≤ 512B**, 넘으면 `ValueTooLarge` 오류, overflow 비트 자리만 예약 | 기준을 "값"에서 "셀 전체"로 바꾸고 키 상한을 더함. SQLite index b-tree도 4KB에서 1002B(약 1/4)로 같은 규칙 |
| 체크섬 | 페이지마다 | **CRC-32C 32비트**, 범위 = 바이트 4..4096 + 페이지 번호, 디스크에서 읽을 때마다 검사, 틀리면 무조건 오류 | 그대로. 페이지 번호 섞기(PG 방식)와 "0 페이지 예외 없음" 규칙 추가 |

**머리 필드 (40바이트)**

| offset | 필드 | 타입 | 용도 |
|---|---|---|---|
| 0 | checksum | u32 | CRC-32C (4절) |
| 4 | page_lsn | u64 | 마지막으로 바꾼 커밋 묶음의 LSN (5절) |
| 12 | kind | u8 | leaf / internal / free |
| 13 | flags | u8 | v1은 0, overflow 비트 자리 예약 |
| 14 | cell_count | u16 | 셀(슬롯) 개수 |
| 16 | free_start | u16 | 슬롯 배열 끝 |
| 18 | free_end | u16 | 셀 영역 시작 |
| 20 | frag_bytes | u16 | 지운 셀이 남긴 빈 바이트 |
| 22 | reserved | u16 | 0 |
| 24 | right_sibling | u64 | leaf의 오른쪽 형제, 없으면 0 |
| 32 | rightmost_child | u64 | internal의 마지막 자식, leaf는 0 |

ADR로 남길 것: 페이지 크기와 메타의 `page_size`, 값·키 상한(되돌리기 어려운 사용자 약속), 체크섬 범위 규칙. 페이지 머리 세부 배치는 파일 형식 버전(`format_version`)으로 관리하면 되므로 ADR까지는 필요 없다.

---

## 미검증

- macOS에 Linux `RWF_ATOMIC`과 같은 "원자적 페이지 쓰기" API가 있는지 1차 출처로 확인하지 못했다. 설계는 원자적이지 않다고 가정하므로 결과에 영향은 없다.
- Apple 공식 문서에서 "Apple silicon VM 페이지 = 16KB"라는 숫자를 직접 찾지 못했다. Apple 문서는 "다르다, `vm_page_size`로 읽어라"까지만 말한다. 16KB는 이 Mac에서 직접 측정한 값이다.
- macOS의 direct I/O(`F_NOCACHE`) 정렬 조건은 확인하지 않았다. v1은 direct I/O를 쓰지 않는다.
- NVMe 규격서 원문(AWUN/AWUPF 정의)은 읽지 않았다. Linux 드라이버 소스에서 어떻게 쓰는지만 확인했다.
- 4KB 쓰기가 macOS 16KB 페이지 캐시에서 읽기-고치기-쓰기 비용을 만드는지(성능)는 측정하지 않았다. 벤치마크(`docs/benchmarks/`)에서 확인할 일이다.
- ARIES 논문 본문(스캔 PDF)은 이번에도 텍스트로 읽지 못했다. page LSN 설명은 PostgreSQL 소스와 0004에서 정리한 초록·강의 노트에 기댄다.
- fan-out·트리 높이 표는 가정한 크기로 계산한 값이다. 실제 셀 형식이 정해지면 다시 계산한다.
- `crc32c` crate의 마지막 릴리스는 2024-06이다. 계속 관리되는지, 표준 intrinsic으로 직접 짜는 편이 나은지는 구현 때 판단한다.

---

## 출처

**SQLite**
- PRAGMA page_size: https://www.sqlite.org/pragma.html#pragma_page_size
- Database File Format: https://www.sqlite.org/fileformat2.html
- Powersafe Overwrite: https://www.sqlite.org/psow.html
- Atomic Commit: https://www.sqlite.org/atomiccommit.html
- Checksum VFS: https://www.sqlite.org/cksumvfs.html
- cksumvfs.c: https://github.com/sqlite/sqlite/blob/master/ext/misc/cksumvfs.c

**PostgreSQL**
- Database Page Layout (18): https://www.postgresql.org/docs/current/storage-page-layout.html
- bufpage.h: https://github.com/postgres/postgres/blob/master/src/include/storage/bufpage.h
- configure.ac (`--with-blocksize`): https://github.com/postgres/postgres/blob/master/configure.ac
- nbtree README: https://github.com/postgres/postgres/blob/master/src/backend/access/nbtree/README
- TOAST: https://www.postgresql.org/docs/current/storage-toast.html
- Data Checksums: https://www.postgresql.org/docs/current/checksums.html
- initdb: https://www.postgresql.org/docs/current/app-initdb.html
- PostgreSQL 18 Release Notes: https://www.postgresql.org/docs/18/release-18.html
- checksum_impl.h: https://github.com/postgres/postgres/blob/master/src/include/storage/checksum_impl.h
- WAL Reliability: https://www.postgresql.org/docs/current/wal-reliability.html
- WAL configuration (full_page_writes, wal_log_hints): https://www.postgresql.org/docs/current/runtime-config-wal.html
- transam README (WAL rule, page LSN): https://github.com/postgres/postgres/blob/master/src/backend/access/transam/README

**MySQL InnoDB (8.4)**
- InnoDB Limits: https://dev.mysql.com/doc/refman/8.4/en/innodb-limits.html
- InnoDB Startup Options (innodb_checksum_algorithm, innodb_doublewrite): https://dev.mysql.com/doc/refman/8.4/en/innodb-parameters.html
- InnoDB Row Formats: https://dev.mysql.com/doc/refman/8.4/en/innodb-row-format.html
- Doublewrite Buffer: https://dev.mysql.com/doc/refman/8.4/en/innodb-doublewrite-buffer.html
- ut/crc32.cc: https://github.com/mysql/mysql-server/blob/8.4/storage/innobase/ut/crc32.cc

**OS / 저장 장치**
- Apple, Addressing architectural differences in your macOS code: https://developer.apple.com/documentation/apple-silicon/addressing-architectural-differences-in-your-macos-code
- Linux arch/Kconfig (PAGE_SIZE_4KB, PAGE_SHIFT): https://github.com/torvalds/linux/blob/master/arch/Kconfig
- Linux arch/x86/Kconfig: https://github.com/torvalds/linux/blob/master/arch/x86/Kconfig
- getpagesize(2): https://git.kernel.org/pub/scm/docs/man-pages/man-pages.git/plain/man/man2/getpagesize.2
- open(2) (O_DIRECT): https://git.kernel.org/pub/scm/docs/man-pages/man-pages.git/plain/man/man2/open.2
- readv(2) (RWF_ATOMIC): https://git.kernel.org/pub/scm/docs/man-pages/man-pages.git/plain/man/man2/readv.2
- statx(2) (STATX_WRITE_ATOMIC, STATX_DIOALIGN): https://git.kernel.org/pub/scm/docs/man-pages/man-pages.git/plain/man/man2/statx.2
- Linux NVMe driver core.c (NAWUPF/AWUPF): https://github.com/torvalds/linux/blob/master/drivers/nvme/host/core.c

**체크섬 / 복구 이론**
- RFC 3720 (CRC32C): https://www.rfc-editor.org/rfc/rfc3720
- Rust `_mm_crc32_u64`: https://doc.rust-lang.org/core/arch/x86_64/fn._mm_crc32_u64.html
- Rust `__crc32cd`: https://doc.rust-lang.org/core/arch/aarch64/fn.__crc32cd.html
- crc32c crate: https://github.com/zowens/crc32c
- ARIES 초록 (Mohan et al., ACM TODS 17(1), 1992): https://www.cs.brandeis.edu/~liuba/abstracts/mohan.html

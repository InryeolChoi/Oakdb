# 0004. B+Tree DB는 정전 뒤에 어떻게 살아나는가: 장애 복구 방식 조사

- 작성일: 2026-10-12
- 관련 이슈: #7 (상위 #1)
- 질문: 페이지를 제자리에서 고쳐 쓰는 B+Tree DB는 정전 뒤 어떻게 복구하나? SQLite(롤백 저널, WAL), PostgreSQL(WAL, full page writes), ARIES는 각각 어떻게 하고, 반쯤 쓴 페이지(torn page)는 어떻게 처리하나? 2개월 프로젝트에서 현실적인 최소 설계는 무엇인가?
- 이미 정해진 전제
  - Rust, B+Tree 키-값 엔진 위의 단순한 관계형 계층 (ADR-0004)
  - 차별점은 동시 쓰기와 네트워크 서버 (ADR-0005)
  - 격리 수준은 snapshot isolation, 내구성은 **group commit**: "커밋됐다"고 응답한 트랜잭션은 정전 뒤에도 남아 있어야 한다
  - macOS와 Linux에서 같은 내구성 약속을 지킨다 (ADR-0003)
- `fsync`의 함정, fsyncgate, macOS `F_FULLFSYNC`, `Fs` trait과 가짜 파일시스템으로 하는 crash 테스트는 [0001 조사](0001-engine-foundation-tradeoffs.md)의 4~6절에 이미 정리했다. 여기서는 반복하지 않고 링크만 건다.
- 표기 규칙 (0001과 같음)
  - **[사실]**: 링크한 1차 출처(공식 문서, 소스 코드, 논문)에서 직접 확인한 내용
  - **[추론]**: 사실을 바탕으로 이 프로젝트에 적용한 해석
  - **[권고]**: 의견. 최종 결정은 ADR로 남긴다
  - **[미검증]**: 1차 출처로 직접 확인하지 못한 내용

---

## 한눈에 보기

1. 디스크에 있는 데이터 파일만으로는 정전 뒤에 무엇이 반쯤 바뀌었는지 알 수 없다. 그래서 모든 DB는 **고치기 전에 메모를 먼저 남긴다**. 메모에 "원래 내용"을 적으면 **되돌리기(undo)**, "새 내용"을 적으면 **다시 하기(redo)**로 복구한다.
2. SQLite 롤백 저널은 원래 내용을, SQLite WAL과 PostgreSQL은 새 내용을, ARIES는 둘 다 적는다. LMDB는 메모 대신 **절대 덮어쓰지 않는** 방법(copy-on-write)을 쓴다.
3. 반쯤 쓴 페이지는 "그 페이지의 **온전한 사본**이 다른 곳에 있다"는 것으로 해결한다. SQLite WAL은 WAL 안의 페이지 전체, PostgreSQL은 full page image, InnoDB는 doublewrite buffer, LMDB는 덮어쓰지 않은 옛 페이지가 그 사본이다.
4. **[권고]** Oakdb의 최소 설계: **트랜잭션이 커밋하기 전에는 트리를 건드리지 않는다**(쓰기를 따로 모아 둔다). 그러면 디스크에 커밋 안 된 데이터가 올라갈 일이 없어서 undo가 필요 없다. 로그에는 **바뀐 페이지 전체(page image)**만 적는 redo 전용 WAL을 쓴다. SQLite WAL과 같은 구조이고, 반쯤 쓴 페이지 문제가 저절로 풀린다.

| 방식 | 대표 | 로그에 적는 것 | 반쯤 쓴 페이지 대책 | Oakdb 2개월 적합도 |
|---|---|---|---|---|
| 롤백 저널 (undo 전용, force) | SQLite 기본 모드 | 고치기 전 원래 페이지 | 원래 페이지가 저널에 있음 | 낮음: 커밋마다 데이터 파일 fsync, group commit과 안 맞음 |
| 페이지 단위 redo 전용 WAL | SQLite WAL 모드 | 고친 뒤 페이지 전체 | 새 페이지가 WAL에 있음 | **높음 (권고)** |
| ARIES (steal/no-force, undo+redo) | DB2, PostgreSQL·InnoDB의 기본 골격 | 바뀐 부분(작은 레코드) | 따로 필요 (FPW, doublewrite) | 낮음: 구현 범위가 너무 큼 |
| copy-on-write, 로그 없음 | LMDB | 없음 | 옛 페이지를 덮어쓰지 않음 | 중간: 쓰기 하나씩만, 매 커밋 데이터 fsync |
| 작은 로그 + copy-on-write 체크포인트 | WiredTiger | 연산(키 단위) | 체크포인트가 옛 블록을 덮어쓰지 않음 | 중간: 좋은 대안이지만 빈 페이지 관리가 복잡 |

---

## 1. 왜 정전이 무서운가

**비유**: 두꺼운 장부를 연필로 고쳐 쓰는 중이라고 하자. 세 페이지를 함께 고쳐야 하는 일(예: 잔액 이동)을 하다가 두 번째 페이지 중간에서 정전이 났다. 불이 다시 들어오면 장부에는 고친 페이지, 반쯤 고친 페이지, 아직 안 고친 페이지가 섞여 있다. 장부만 봐서는 무엇이 원래 내용이었는지 알 수 없다.

용어 한 줄 정의:

- **페이지(page)**: DB 파일을 같은 크기(예: 4 KiB)로 자른 한 조각. B+Tree의 노드 하나가 페이지 하나다.
- **제자리 수정(in-place update)**: 페이지를 고칠 때 원래 자리에 덮어쓰는 방식. Oakdb의 B+Tree가 이렇게 한다 (ADR-0004).
- **torn page(반쯤 쓴 페이지)**: 페이지 하나를 쓰는 도중 전원이 나가서, 앞부분은 새 내용이고 뒷부분은 옛 내용인 페이지.
- **로그(log, journal)**: 데이터를 고치기 전에 "무엇을 할지"를 순서대로 적어 두는 별도 파일. 끝에만 덧붙인다.
- **undo / redo**: undo는 끝나지 않은 일을 원래대로 되돌리기, redo는 끝난 일을 다시 해서 디스크에 반영하기.
- **체크포인트(checkpoint)**: "여기까지는 데이터 파일에 다 반영됐다"고 표시하는 지점. 이 앞의 로그는 지워도 된다.
- **LSN(log sequence number)**: 로그 레코드마다 붙는, 계속 커지는 번호. "이 페이지는 몇 번 로그까지 반영됐다"를 표시할 때 쓴다.
- **fsync**: OS에 "지금까지 쓴 것을 진짜 디스크에 내려 달라"고 요청하는 호출. 함정은 [0001 4·5절](0001-engine-foundation-tradeoffs.md) 참조.

디스크가 어떤 단위까지 "한 번에" 쓰는지도 중요하다.

- [사실] SQLite는 디스크가 **섹터(sector)** 단위로 쓰인다고 가정하고, 섹터 쓰기조차 원자적(전부 아니면 전무)이라고 가정하지 않는다 ([SQLite Atomic Commit §2](https://www.sqlite.org/atomiccommit.html)).
- [사실] PostgreSQL은 보통 8192 바이트(512 바이트 섹터 16개)를 한 번에 쓰는데, 전원이 나가면 그중 일부 섹터만 써질 수 있다고 설명한다 ([PostgreSQL WAL Reliability](https://www.postgresql.org/docs/current/wal-reliability.html)).
- [사실] SQLite의 정의: torn page는 DB 페이지가 디스크 섹터보다 크고, 페이지의 모든 섹터가 써지기 전에 전원이 나간 경우다 ([SQLite PSOW](https://www.sqlite.org/psow.html)).

즉 **페이지 하나 쓰기는 원자적이지 않다**고 가정하고 설계해야 한다.

---

## 2. SQLite: 롤백 저널과 WAL

### 2.1 롤백 저널 (기본 모드): "원본을 먼저 복사해 두기"

**비유**: 장부를 고치기 전에, 고칠 페이지를 복사기로 떠서 따로 둔다. 다 고치고 나면 복사본을 버린다. 중간에 정전이 나면 복사본을 장부에 다시 붙여 넣는다.

- [사실] 커밋 순서 ([Atomic Commit §3.5~3.11](https://www.sqlite.org/atomiccommit.html)):
  1. 바뀔 페이지의 **원래 내용**을 롤백 저널 파일에 쓴다.
  2. 저널을 fsync한다. 대부분의 플랫폼에서 fsync가 두 번 필요하다.
  3. DB 파일에 새 내용을 쓴다.
  4. DB 파일을 다시 fsync한다.
  5. 저널을 지운다. **"This is the instant where the transaction commits."** 저널 헤더를 0으로 채우거나 파일 길이를 0으로 줄이는 것으로 대신할 수도 있다.
- [사실] 복구: 다음에 DB를 여는 프로세스가 **hot journal**(존재하고, 비어 있지 않고, 헤더가 멀쩡하고, 아무도 쓰기 잠금을 잡지 않은 저널)을 발견하면 원래 페이지를 DB 파일 제자리에 되써 넣고, 파일 크기를 원래대로 자른 뒤 저널을 지운다. 프로그램 입장에서는 그 트랜잭션이 처음부터 없었던 것처럼 보인다 ([Atomic Commit §4](https://www.sqlite.org/atomiccommit.html)).
- [사실] 저널 자체가 깨지는 경우의 대책: 저널 헤더의 페이지 개수는 처음에 0으로 써 두고, 저널 내용을 fsync한 **다음에** 실제 개수로 바꾼다. 또 저널의 페이지마다 32비트 체크섬이 있고, 트랜잭션마다 다른 난수(nonce)로 체크섬을 시작한다. 이전 저널의 찌꺼기가 우연히 맞아떨어지는 것을 막기 위해서다 ([Atomic Commit §6.2](https://www.sqlite.org/atomiccommit.html), [File Format §3](https://www.sqlite.org/fileformat2.html)).
- [추론] 이것은 **undo 전용 + force** 방식이다. 커밋할 때마다 DB 파일의 바뀐 페이지를 전부 디스크에 내려야 하고(force), fsync가 최소 두세 번 든다. 여러 트랜잭션을 한 번의 fsync로 묶는 group commit과 맞지 않는다.

### 2.2 WAL 모드: "새 내용을 뒤에 덧붙이기"

**비유**: 장부는 그대로 두고, 고친 페이지를 새로 써서 장부 뒤 "추가 노트"에 끼워 둔다. 읽을 때는 추가 노트에 최신 페이지가 있으면 그것을 본다. 가끔 한가할 때 추가 노트 내용을 장부 본문에 옮겨 적고 노트를 비운다(체크포인트).

- [사실] 원래 내용은 DB 파일에 그대로 두고, 바뀐 내용을 별도 WAL 파일 끝에 덧붙인다. **커밋 표시 레코드가 WAL에 덧붙는 순간이 커밋**이다. 그래서 DB 파일을 한 번도 건드리지 않고 커밋할 수 있다 ([SQLite WAL](https://www.sqlite.org/wal.html)).
- [사실] WAL 파일 형식: 32바이트 헤더(매직 넘버, 페이지 크기, 체크포인트 순번, salt 두 개, 헤더 체크섬) 뒤에 **프레임**이 이어진다. 프레임 하나는 24바이트 헤더 + **페이지 전체**다. 프레임 헤더에는 페이지 번호, 커밋 프레임이면 커밋 뒤 DB 크기(아니면 0), salt, 그리고 WAL 맨 앞부터 이 프레임까지 **누적한 체크섬**이 들어간다 ([File Format §4.1](https://www.sqlite.org/fileformat2.html)).
- [사실] 프레임은 salt가 헤더와 같고 누적 체크섬이 맞을 때만 유효하다. 페이지를 읽을 때는 "커밋 프레임이거나 뒤에 커밋 프레임이 오는 유효한 프레임 중 마지막 것"을 쓰고, 없으면 DB 파일에서 읽는다 ([File Format §4.1](https://www.sqlite.org/fileformat2.html)).
- [사실] 체크포인트가 끝난 뒤 새 쓰기를 시작할 때 salt를 바꾼다. 그러면 예전 프레임이 파일에 남아 있어도 무효가 되어 다시 반영되지 않는다 ([File Format §4.1](https://www.sqlite.org/fileformat2.html)).
- [사실] 기본적으로 WAL이 1000 페이지가 되면 자동으로 체크포인트를 한다 ([SQLite WAL](https://www.sqlite.org/wal.html)).
- [사실] WAL 모드에서 `synchronous=NORMAL`이면 WAL은 체크포인트 전에, DB 파일은 체크포인트가 끝난 뒤에, WAL 헤더는 WAL을 다시 쓰기 시작할 때 fsync한다. 대부분의 커밋에서는 fsync를 하지 않으므로 정전 뒤 마지막 커밋이 사라질 수 있다. `FULL`이면 커밋마다 WAL을 fsync하고, WAL 모드에서 ACID가 된다 ([SQLite PRAGMA synchronous](https://www.sqlite.org/pragma.html)).
- [사실] 읽는 쪽은 시작할 때 "마지막 유효 커밋 위치(end mark)"를 기억하고 끝날 때까지 그 시점의 DB만 본다. WAL 파일이 하나뿐이라 쓰기는 한 번에 하나다. 읽는 트랜잭션이 끊이지 않으면 체크포인트가 끝나지 못해 WAL이 끝없이 커질 수 있다(checkpoint starvation) ([SQLite WAL](https://www.sqlite.org/wal.html)).

### 2.3 SQLite가 반쯤 쓴 페이지를 다루는 법

- [사실] "SQLite never assumes that database page writes are atomic, regardless of the PSOW setting. And hence SQLite is always able to automatically recover from torn pages induced by a crash." ([SQLite PSOW](https://www.sqlite.org/psow.html))
- [사실] **PSOW(powersafe overwrite)**: "파일의 어떤 범위를 쓰다가 전원이 나가도, 그 범위 **밖의** 바이트는 바뀌지 않는다"는 성질이다. 쓰던 범위 안의 바이트는 옛 값, 새 값, 쓰레기 중 무엇이든 될 수 있다. 3.7.10부터 기본값은 켜짐이다. 꺼져 있으면 SQLite는 같은 섹터에 있는 바이트까지 함께 저널에 남기고, 저널을 섹터 경계까지 채워 다음 덧붙이기가 앞 레코드를 망가뜨리지 않게 한다 ([SQLite PSOW](https://www.sqlite.org/psow.html)).
- [추론] 정리하면 SQLite는 두 모드 모두 "**페이지 전체 사본이 로그에 있다**"로 torn page를 해결한다. 롤백 저널은 옛 페이지 전체를, WAL은 새 페이지 전체를 갖고 있다. 체크포인트 도중 DB 파일 페이지가 찢어져도 WAL을 지우기 전이므로 다시 옮겨 쓰면 된다. DB 파일 fsync가 끝난 뒤에야 WAL을 다시 쓰기 시작한다는 위의 `synchronous` 설명이 이 순서를 뒷받침한다.

---

## 3. PostgreSQL: WAL, 체크포인트, full page writes

**비유**: 장부 옆에 "작업 일지"를 둔다. 일지에는 "3번 페이지 7번째 줄을 X로 바꿈"처럼 **바뀐 부분만** 짧게 적는다. 일지만 확실히 금고(디스크)에 넣으면 장부는 나중에 천천히 고쳐도 된다.

- [사실] 핵심 규칙: 데이터 파일의 변경은 그 변경을 설명하는 WAL 레코드가 영구 저장소에 내려간 **뒤에만** 쓸 수 있다. 그래서 커밋마다 데이터 페이지를 내릴 필요가 없고, 크래시 뒤에는 WAL로 다시 하면 된다(roll-forward, REDO). 동시에 작은 트랜잭션이 많으면 WAL fsync 한 번으로 여러 트랜잭션을 커밋할 수 있다 ([PostgreSQL WAL Intro](https://www.postgresql.org/docs/current/wal-intro.html)).
- [사실] 체크포인트: 체크포인트 이전에 WAL에 적힌 내용이 모두 데이터 파일에 반영됐음을 보장하는 지점이다. 이때 dirty 페이지를 모두 디스크에 내리고 체크포인트 레코드를 WAL에 쓴다. 복구는 마지막 체크포인트 레코드가 가리키는 **redo 지점**부터 시작하고, 그 앞의 WAL 세그먼트는 재사용하거나 지울 수 있다. 쓰기 폭주를 막기 위해 체크포인트의 쓰기는 `checkpoint_completion_target`만큼 시간에 나눠서 한다 ([PostgreSQL WAL Configuration](https://www.postgresql.org/docs/current/wal-configuration.html)). 기본값은 `checkpoint_timeout=5min`, `max_wal_size=1GB`다 ([PostgreSQL WAL settings](https://www.postgresql.org/docs/current/runtime-config-wal.html)).
- [사실] **full_page_writes**(기본 켜짐): 체크포인트 뒤 어떤 페이지를 **처음** 고칠 때, 그 페이지 내용 전체를 WAL에 쓴다. 이유: OS 크래시 중에 쓰던 페이지는 옛 데이터와 새 데이터가 섞일 수 있고, 행 단위 변경 기록만으로는 그런 페이지를 되살릴 수 없기 때문이다. WAL 재생은 항상 체크포인트에서 시작하므로 체크포인트 뒤 첫 변경 때만 하면 충분하다. 끄면 빨라지지만 복구 불가능한 손상이나 조용한 손상이 생길 수 있다 ([PostgreSQL WAL settings](https://www.postgresql.org/docs/current/runtime-config-wal.html)).
- [사실] ZFS처럼 부분 페이지 쓰기를 막는 파일시스템이면 이 기능을 꺼도 된다. 배터리 달린 디스크 컨트롤러(BBU)는 8 kB 페이지 전체를 보장하지 않는 한 부분 쓰기를 막지 못한다 ([PostgreSQL WAL Reliability](https://www.postgresql.org/docs/current/wal-reliability.html)).
- [사실] WAL 레코드마다 CRC-32C가 있고, 크래시 복구 때 검사한다. 데이터 페이지도 기본으로 체크섬을 갖고, WAL 안의 full page image는 항상 체크섬으로 보호된다 ([PostgreSQL WAL Reliability](https://www.postgresql.org/docs/current/wal-reliability.html)). PostgreSQL 18 문서 기준 데이터 체크섬은 기본으로 켜져 있고 클러스터 단위로 끌 수 있다 ([PostgreSQL Data Checksums](https://www.postgresql.org/docs/current/checksums.html)).
- [사실] 페이지 헤더의 `pd_lsn`은 "이 페이지를 마지막으로 바꾼 WAL 레코드의 끝 다음 바이트"이고, `pd_checksum`은 페이지 체크섬이다 ([PostgreSQL Page Layout](https://www.postgresql.org/docs/current/storage-page-layout.html)).
- [추론] PostgreSQL은 체크포인트 간격과 full page image 양이 서로 맞물린다. 체크포인트를 자주 하면 복구는 빨라지지만 "첫 변경"이 자주 생겨 WAL이 늘어난다 ([WAL Configuration](https://www.postgresql.org/docs/current/wal-configuration.html)도 같은 말을 한다).

---

## 4. ARIES: 교과서의 "정답"

**비유**: 은행의 정식 작업 일지. 모든 변경을 "원래 값과 새 값" 둘 다 적고, 되돌린 작업까지도 "되돌렸음"이라고 적는다. 그래서 복구하다가 또 정전이 나도 같은 일을 두 번 하지 않는다.

> 출처 메모: ARIES 논문 PDF는 스캔 이미지라 이번에 본문 글자를 직접 뽑지 못했다. 아래는 논문 초록과 CMU 15-445 강의 노트(ARIES를 그대로 가르치는 자료)로 확인한 내용이다. 강의 노트는 "모든 시스템이 논문 그대로 구현하지는 않는다"고 밝힌다.

먼저 버퍼 관리 정책 두 가지를 정의한다.

- [사실] **steal**: 커밋 안 된 트랜잭션의 변경을 디스크에 써도 되는가. 허용하면 steal, 아니면 no-steal. **force**: 커밋하기 전에 트랜잭션의 변경을 디스크에 모두 반영해야 하는가. 강제하면 force, 아니면 no-force. **가장 구현하기 쉬운 조합은 no-steal + force**다. undo도 redo도 필요 없지만, 트랜잭션이 고칠 데이터가 메모리에 다 들어가야 한다 ([CMU 15-445 F19 Lecture #20](https://15445.courses.cs.cmu.edu/fall2019/notes/20-logging.pdf)).
- [사실] steal이면 디스크에 커밋 안 된 내용이 있을 수 있으니 undo가 필요하고, no-force면 커밋된 내용이 로그에만 있을 수 있으니 redo가 필요하다. WAL은 steal + no-force 방식이고, 런타임 성능이 가장 좋아 거의 모든 DBMS가 쓴다. 대신 복구는 로그를 재생해야 해서 shadow paging보다 느리다 ([CMU Lecture #20](https://15445.courses.cs.cmu.edu/fall2019/notes/20-logging.pdf)).

ARIES의 핵심:

- [사실] ARIES는 WAL을 쓰는 복구 방법이고, 페이지마다 LSN을 둬서 페이지 상태와 로그를 연결한다. 재시작할 때 **"repeating history"**로 빠진 변경을 모두 다시 한 뒤 끝나지 않은 트랜잭션을 되돌린다. 되돌리는 동안 한 변경도 모두 로그에 남긴다. fuzzy checkpoint를 지원하고, 버퍼 관리 정책에 유연하다 ([ARIES 초록](https://www.cs.brandeis.edu/~liuba/abstracts/mohan.html), [논문](https://dsf.berkeley.edu/cs262/Aries.pdf)).
- [사실] **WAL 규칙**: 페이지 i를 디스크에 쓰기 전에 로그를 최소한 `pageLSN_i ≤ flushedLSN`이 될 때까지 내려야 한다. 커밋은 COMMIT 레코드까지 로그를 디스크에 내린 뒤에야 응답한다 ([CMU 15-445 F19 Lecture #21](https://15445.courses.cs.cmu.edu/fall2019/notes/21-recovery.pdf)).
- [사실] **CLR(compensation log record)**: undo한 동작을 기록하는 레코드. `undoNext`(다음에 되돌릴 LSN)를 갖고, 그 자신은 절대 undo하지 않는다 ([CMU Lecture #21](https://15445.courses.cs.cmu.edu/fall2019/notes/21-recovery.pdf)).
- [사실] **fuzzy checkpoint**: 트랜잭션을 멈추지 않고 `CHECKPOINT-BEGIN`과 `CHECKPOINT-END` 레코드를 쓴다. END에는 활성 트랜잭션 표(ATT: 트랜잭션 id, 상태, lastLSN)와 dirty page 표(DPT: 페이지마다 처음 dirty하게 만든 레코드의 recLSN)가 들어간다 ([CMU Lecture #21](https://15445.courses.cs.cmu.edu/fall2019/notes/21-recovery.pdf)).
- [사실] **복구 3단계** ([CMU Lecture #21](https://15445.courses.cs.cmu.edu/fall2019/notes/21-recovery.pdf)):
  1. **Analysis**: MasterRecord가 가리키는 마지막 체크포인트부터 로그를 읽어 크래시 순간의 ATT와 DPT를 다시 만든다.
  2. **Redo**: DPT의 가장 작은 recLSN부터 앞으로 읽으며, 중단된 트랜잭션 것까지 **모든** 변경과 CLR을 다시 적용한다. 단 페이지가 DPT에 없거나, 디스크의 pageLSN이 레코드 LSN 이상이면 건너뛴다.
  3. **Undo**: 크래시 때 끝나지 않은 트랜잭션을 LSN 역순으로 되돌리며 CLR을 쓴다.
- [미검증] ARIES 논문이 torn page를 어떻게 다루는지는 본문을 직접 읽지 못해 확인하지 못했다. [추론] 레코드 단위 redo는 "페이지가 어떤 온전한 상태에 있다"는 전제 위에서 그 페이지에 변경을 덧입히는 것이므로, 페이지가 반쯤 찢어져 있으면 그 전제가 깨진다. PostgreSQL이 full page writes를, InnoDB가 doublewrite를 따로 두는 이유가 이것이다(3절, 5절).

---

## 5. InnoDB: redo log + doublewrite buffer

- [사실] redo log는 크래시 복구 때 끝나지 않은 트랜잭션이 쓴 데이터를 바로잡는 디스크 구조다. 계속 커지는 LSN으로 위치를 나타내고, 체크포인트 때 체크포인트 LSN을 기록해서 복구는 마지막 체크포인트 LSN부터 시작한다 ([InnoDB Redo Log](https://dev.mysql.com/doc/refman/8.4/en/innodb-redo-log.html)).
- [사실] **doublewrite buffer**: 버퍼 풀의 페이지를 데이터 파일 제자리에 쓰기 **전에** 먼저 별도 영역에 쓴다. 페이지를 쓰다가 OS나 저장장치, 프로세스가 죽으면 복구 때 doublewrite buffer에서 온전한 사본을 찾는다. 두 번 쓰지만 I/O가 두 배는 아니다. 큰 순차 덩어리로 쓰고 fsync는 한 번이다. `DETECT_ONLY` 모드는 내용 없이 메타데이터만 써서 부분 쓰기를 **감지만** 하고 고치지는 않는다. Fusion-io의 원자적 쓰기를 지원하는 장치에서는 자동으로 꺼진다 ([InnoDB Doublewrite Buffer](https://dev.mysql.com/doc/refman/8.4/en/innodb-doublewrite-buffer.html)).
- [추론] PostgreSQL은 "온전한 사본"을 WAL 안에 두고(full page image), InnoDB는 별도 파일에 둔다(doublewrite). 둘 다 같은 질문에 대한 다른 답이다: 페이지를 제자리에 덮어쓰다 찢어지면, 어디서 온전한 판본을 가져올 것인가.

---

## 6. 로그 없이 하는 방법: LMDB의 copy-on-write, WiredTiger의 체크포인트

**비유**: 장부의 페이지를 고칠 때 원래 페이지에 지우개를 대지 않는다. 새 종이에 고친 페이지를 쓰고, 마지막에 맨 앞 "목차"만 새 페이지를 가리키게 바꾼다. 목차를 바꾸기 전에 정전이 나면 옛 목차가 옛 페이지들을 그대로 가리키고 있다.

### LMDB

- [사실] LMDB는 "crash-proof" 키-값 저장소이고, 로그나 크래시 복구 절차가 필요 없다고 말한다. MVCC로 읽기와 쓰기가 서로 막지 않고, 쓰기는 직렬화된다 ([Symas LMDB](https://www.symas.com/mdb)).
- [사실] 파일의 0번과 1번 페이지가 **메타 페이지 두 개**이고, 트랜잭션이 번갈아 쓴다. 각 메타 페이지는 데이터 B+Tree와 빈 페이지 목록(free list) B+Tree의 루트를 가리킨다. 커밋 때 더 오래된 쪽 메타 페이지를 덮어쓰고, DB를 여는 쪽은 더 새 메타 페이지를 쓴다. 쓰기는 copy-on-write라서 페이지를 고칠 때 복사본을 고친다. 쓰기는 한 번에 하나다 ([Chu, MDB paper §3.2, §4.2](https://mail.openldap.org/pub/hyc/mdb-paper.pdf)).
- [사실] 커밋 코드는 바뀐 페이지를 쓰고(`mdb_page_flush`), 데이터 파일을 sync한(`mdb_env_sync0`) **다음에** 메타 페이지를 쓴다(`mdb_env_write_meta`) ([LMDB mdb.c](https://github.com/LMDB/lmdb/blob/mdb.master/libraries/liblmdb/mdb.c)).
- [사실] `MDB_NOMETASYNC`는 메타 페이지 sync를 생략한다. 무결성은 지키지만 시스템 크래시 때 마지막 커밋이 사라질 수 있다(ACI는 지키고 D는 잃는다). `MDB_PREVSNAPSHOT`은 이전 메타 페이지로 열어 일부 손상을 피할 수 있다 ([lmdb.h](https://github.com/LMDB/lmdb/blob/mdb.master/libraries/liblmdb/lmdb.h)).
- [사실] 오래 열린 읽기 트랜잭션이 있으면 새 쓰기가 풀어 준 페이지를 재사용하지 못해 DB가 빠르게 커질 수 있다 ([lmdb.h caveats](https://github.com/LMDB/lmdb/blob/mdb.master/libraries/liblmdb/lmdb.h), [MDB paper §4.2](https://mail.openldap.org/pub/hyc/mdb-paper.pdf)).
- [추론] 리프 하나를 copy-on-write하면 그 부모가 새 주소를 가리켜야 하므로 부모도 복사해야 하고, 결국 루트까지 복사된다. 그래서 커밋마다 데이터 파일 fsync + 메타 fsync가 필요하고, 여러 트랜잭션을 한 메타 교체에 묶지 않으면 group commit 효과가 작다. 쓰기가 한 번에 하나인 것도 이 구조와 맞물린다.

### WiredTiger (MongoDB의 저장 엔진)

- [사실] 체크포인트는 트랜잭션 일관된 스냅샷을 쓰는 것이고, 그 전에 커밋된 트랜잭션은 체크포인트로 내구성을 얻는다. 체크포인트끼리 파일 블록을 공유할 수 있다 ([WiredTiger Checkpoint (2.8.0)](https://source.wiredtiger.com/2.8.0/checkpoint.html), [WiredTiger arch-checkpoint](https://source.wiredtiger.com/develop/arch-checkpoint.html)).
- [사실] 크래시 뒤에는 마지막 체크포인트에서 출발해 로그를 재생한다 ([WiredTiger arch-checkpoint](https://source.wiredtiger.com/develop/arch-checkpoint.html)). MongoDB는 데이터 파일에서 마지막 체크포인트 식별자를 찾고, 저널에서 그에 맞는 레코드를 찾아, 그 뒤 연산을 적용한다고 설명한다. 저널 레코드는 클라이언트 쓰기 연산마다 하나다 ([MongoDB Journaling](https://www.mongodb.com/docs/v8.0/core/journaling/)).
- [추론] WiredTiger식은 "**로그에는 키 단위 연산만, 데이터 파일은 copy-on-write 체크포인트로만 바꾼다**"이다. 로그가 작아 group commit에 유리하고 torn page 걱정이 없다. 대신 체크포인트의 블록 재사용(어느 블록을 언제 다시 써도 되나) 관리가 필요하다.

---

## 7. 반쯤 쓴 페이지(torn page) 대책 모아 보기

| 시스템 | 온전한 사본은 어디에? | 감지 수단 |
|---|---|---|
| SQLite 롤백 저널 | 저널 안의 **옛** 페이지 전체 | 저널 페이지 체크섬 + nonce, 헤더 페이지 수 |
| SQLite WAL | WAL 안의 **새** 페이지 전체 (체크포인트가 끝나 DB 파일이 sync될 때까지 보관) | 누적 체크섬 + salt |
| PostgreSQL | 체크포인트 뒤 첫 변경 때 WAL에 쓴 full page image | WAL CRC-32C, 페이지 체크섬 |
| InnoDB | doublewrite buffer | doublewrite 메타데이터 |
| LMDB | 덮어쓰지 않은 옛 페이지 (옛 메타 페이지가 가리킴) | 메타 페이지 두 개 중 새 쪽 선택 |
| WiredTiger | 이전 체크포인트의 블록 | 체크포인트 메타데이터 |

[사실] 출처는 각 절 참조. [추론] 공통 원칙은 하나다: **찢어질 수 있는 쓰기는, 그 페이지의 온전한 판본이 다른 곳에 안전하게(fsync까지) 있을 때만 한다.** 체크섬은 "찢어졌다"는 것을 알려 줄 뿐, 고쳐 주지는 않는다.

---

## 8. 세 가지 설계 비교

여기서 비교하는 세 가지:

- **(가) redo 전용 + no-steal**: 커밋 안 된 내용은 디스크 데이터 파일에 절대 쓰지 않는다. 로그에는 새 내용만 적는다. SQLite WAL이 이 계열이다.
- **(나) ARIES (steal/no-force)**: 무엇이든 언제든 디스크에 쓸 수 있고, 로그에 옛 값과 새 값을 둘 다 적는다.
- **(다) shadow paging / copy-on-write**: 덮어쓰지 않고 새 위치에 쓴 뒤 루트만 바꾼다. LMDB 계열. 강의 노트는 이것을 no-steal + force의 예로 든다 ([CMU Lecture #20](https://15445.courses.cs.cmu.edu/fall2019/notes/20-logging.pdf)).

| | (가) redo 전용 + no-steal | (나) ARIES steal/no-force | (다) shadow paging / COW |
|---|---|---|---|
| 복구 단계 | redo 하나 (로그 앞→뒤로 덮어쓰기) [추론] | analysis, redo, undo 세 단계 + CLR [사실: CMU #21] | 거의 없음: 유효한 메타/루트 고르기 [사실: LMDB] |
| 구현 범위 | 작음. 로그 레코드 종류가 2~3개 [추론] | 큼. LSN 4종(page/flushed/rec/last), ATT/DPT, CLR, 레코드 종류마다 undo/redo 함수 [사실: CMU #21] | 중간. 부모-루트 복사 전파, 빈 페이지 회수 [사실: MDB paper / 추론] |
| 메모리 제약 | 커밋 안 된 변경이 메모리에 다 들어가야 함 [사실: CMU #20] | 없음 [사실: CMU #20] | 커밋 전까지 새 페이지를 어디에 쓰느냐에 따라 다름 [추론] |
| group commit | 잘 맞음. 커밋 = 로그 append + fsync 한 번 [사실: PG WAL intro, 같은 원리] | 잘 맞음 [사실: CMU #20 "group commit"] | 어려움. 커밋마다 데이터+메타 fsync, 여러 트랜잭션을 한 루트 교체로 묶어야 함 [추론] |
| MVCC / snapshot isolation | 트랜잭션의 쓰기를 커밋 때까지 따로 모아 두면 트리에는 **커밋된 버전만** 들어간다 → undo가 필요 없어짐 [추론] | 트리에 커밋 안 된 버전이 들어가므로 abort 때 undo 필요. InnoDB는 undo log로 옛 버전도 만든다 [사실: 0001의 InnoDB MVCC] | 스냅샷 = 옛 루트. MVCC가 공짜에 가까움. 대신 오래 읽으면 공간이 안 풀림 [사실: lmdb.h] |
| torn page | 로그에 페이지 전체를 적으면 자동 해결 [사실: SQLite PSOW] | 별도 장치 필요 (FPW / doublewrite) [사실: PG, InnoDB] | 덮어쓰지 않으므로 데이터 페이지는 안전. 메타 페이지만 이중화 [사실: LMDB] |
| 2개월 AI 작성 Rust | **적합** | 부적합: 테스트할 상태 조합이 너무 많음 [추론] | 가능하지만 동시 쓰기와 충돌 [추론] |

[추론] Oakdb에서 가장 중요한 관찰: **snapshot isolation을 "쓰기를 모아 두었다가 커밋 때 한꺼번에 반영"하는 방식(deferred update)으로 구현하면, (가)의 약점인 "no-steal"이 사실상 공짜가 된다.** 트랜잭션이 실행되는 동안 쓰기는 트랜잭션 개인 버퍼(write set)에만 있고, 트리 페이지에는 커밋이 확정된 버전만 들어간다. 그러면

- abort는 버퍼를 버리면 끝이다. 로그에 아무것도 안 남긴다.
- 디스크 데이터 파일에는 커밋 안 된 데이터가 올라갈 수 없으니 undo가 필요 없다.
- 다만 "커밋은 됐지만 아직 로그가 fsync되지 않은" 페이지는 있을 수 있으므로, ARIES의 WAL 규칙(`pageLSN ≤ flushedLSN`일 때만 페이지를 데이터 파일에 쓴다)은 그대로 지켜야 한다.

---

## 9. Oakdb 선택지 평가 (2개월, 코드는 AI가 작성)

별점은 [추론]이다. 기준: 구현량, 테스트로 정확성을 보이기 쉬운가, group commit·동시 쓰기·snapshot isolation과 맞는가, torn page를 자동으로 막는가.

| # | 선택지 | 구현량 | group commit | 동시 쓰기 / SI | torn page | 종합 |
|---|---|---|---|---|---|---|
| A | SQLite식 롤백 저널 | 작음 | 나쁨 (커밋마다 데이터 파일 fsync) | 나쁨 (쓰기 하나씩) | 자동 | ★★☆☆☆ |
| B | 정식 ARIES + full page writes | 매우 큼 | 좋음 | 좋음 | FPW 추가 필요 | ★☆☆☆☆ |
| **C** | **페이지 이미지 redo 전용 WAL + 커밋 때 반영 (SQLite WAL식)** | **작음~중간** | **좋음** | **좋음** | **자동** | **★★★★★** |
| D | 키 단위 로그 + copy-on-write 체크포인트 (WiredTiger식) | 중간~큼 | 아주 좋음 (로그가 작음) | 좋음 | 자동 | ★★★☆☆ |
| E | C에서 출발해 나중에 "체크포인트 뒤 첫 변경만 전체 페이지, 나머지는 바뀐 부분만"(PostgreSQL식)으로 바꾸기 | C + 중간 | 좋음 | 좋음 | FPW로 해결 | C 다음 단계 |

- **A**는 ADR-0005의 차별점(동시 쓰기)과 group commit 약속에 정면으로 어긋난다.
- **B**는 학습 가치는 크지만 undo, CLR, ATT/DPT를 AI가 짠 코드로 2개월 안에 crash 테스트까지 통과시키기 어렵다.
- **C**의 약점은 **로그 양**이다. 4 KiB 페이지 안의 16바이트 값 하나를 바꿔도 4 KiB를 로그에 쓴다. 같은 group commit 묶음 안에서 같은 페이지는 한 번만 쓰면 이 비용이 줄어든다. 벤치마크에서 문제가 되면 E로 옮긴다.
- **D**는 좋은 설계지만 B+Tree를 copy-on-write로 바꾸는 것이라 ADR-0004의 "제자리 수정" 전제와 다르고, 빈 블록 회수가 추가된다.

---

## 10. [권고] Oakdb 최소 설계

### 10.1 원칙 다섯 줄

1. **커밋 전에는 트리를 건드리지 않는다.** 쓰기는 트랜잭션의 write set에 모았다가, 충돌 검사를 통과하면 커밋 때 트리에 반영한다.
2. **로그에는 바뀐 페이지 전체만 적는다**(redo 전용, physical page image). undo 레코드는 없다.
3. **데이터 파일은 체크포인트 때만 쓴다.** 그 페이지의 전체 이미지가 이미 fsync된 WAL에 있을 때만 쓴다.
4. **메타 정보(체크포인트 위치, 루트, 페이지 수)는 두 칸에 번갈아 쓴다.** LMDB의 메타 페이지 두 개 방식이다.
5. **fsync가 실패하면 엔진을 멈춘다.** 재시도하지 않는다 ([0001 4절](0001-engine-foundation-tradeoffs.md) fsyncgate 교훈).

### 10.2 파일 구성

```
data/
  oak.db              # 페이지 배열. 0번, 1번 페이지 = 메타 슬롯 A/B
  wal/
    00000000000000000001.wal   # WAL 세그먼트. 끝까지 차면 새 파일. 재사용하지 않음
    00000000000000000002.wal
```

- 페이지 크기는 4096 바이트로 시작한다. [사실] 요즘 드라이브의 섹터가 4096 바이트라 SQLite도 PSOW를 기본으로 켠다 ([SQLite PSOW](https://www.sqlite.org/psow.html)). 그래도 페이지 쓰기가 원자적이라고는 가정하지 않는다.
- WAL 세그먼트는 재사용하지 않고 새로 만들고 지운다. 그러면 SQLite의 salt처럼 "옛 프레임이 섞이는" 문제를 크게 줄일 수 있다. 다만 파일을 만들거나 지운 뒤에는 디렉터리를 fsync한다 ([0001 4절](0001-engine-foundation-tradeoffs.md)).

### 10.3 데이터 페이지와 메타 슬롯 형식 (스케치)

```
데이터 페이지 (4096 B)
  0..4    crc32c        # 나머지 4092 바이트의 체크섬
  4..12   page_lsn u64  # 이 페이지를 마지막으로 바꾼 커밋 묶음의 LSN
  12..13  kind u8       # leaf / internal / free
  ...     B+Tree 노드 내용 (키, 커밋 타임스탬프, 값)

메타 슬롯 (0번, 1번 페이지)
  magic "OAKMETA1", format_version u32
  meta_seq u64          # 쓸 때마다 +1. 둘 중 crc가 맞고 큰 쪽이 현재
  checkpoint_lsn u64    # 이 LSN까지의 변경은 oak.db에 모두 반영되고 fsync됨
  root_page u64, page_count u64
  max_commit_ts u64     # 재시작 뒤 타임스탬프를 이보다 크게 시작
  crc32c u32
```

### 10.4 WAL 레코드 형식 (스케치)

```
세그먼트 헤더 (32 B)
  magic "OAKWAL01" | version u32 | page_size u32 | segment_no u64 | first_lsn u64 | crc32c u32

레코드 (공통 머리 21 B + 본문)
  len u32        # 본문 길이
  crc32c u32     # kind + lsn + 본문 + segment_no 에 대한 체크섬
  kind u8        # 1 = PAGE, 2 = COMMIT
  lsn u64        # 바이트 오프셋 기반, 계속 증가

PAGE 본문:   page_no u64 | image [page_size]
COMMIT 본문: batch_no u64 | n_pages u32 | page_count u64 | max_commit_ts u64 | n_txns u32
```

- 한 group commit 묶음 = PAGE 레코드 여러 개 + COMMIT 레코드 하나. **COMMIT 레코드가 있어야 앞의 PAGE들이 유효하다.** SQLite의 "커밋 프레임 뒤에 오는 프레임은 무시" 규칙과 같다 ([File Format §4.1](https://www.sqlite.org/fileformat2.html)).
- CRC는 Castagnoli(CRC-32C)를 쓴다. PostgreSQL WAL과 같다 ([WAL Reliability](https://www.postgresql.org/docs/current/wal-reliability.html)).
- 체크섬에 `segment_no`를 넣어서 다른 세그먼트의 레코드가 우연히 맞아떨어지지 않게 한다. SQLite nonce/salt와 같은 목적이다.

### 10.5 커밋 경로 (group commit)

1. 트랜잭션은 여러 스레드에서 **동시에** 실행된다. 읽기는 시작 시점 스냅샷(durable 커밋 타임스탬프)으로, 쓰기는 write set에 모은다.
2. 커밋 요청은 **커밋 큐**에 들어간다.
3. **커밋 담당 스레드 하나**가 큐에서 지금 쌓인 요청을 한 번에 꺼낸다(일부러 기다리지 않음, RocksDB 방식, [0001 4절](0001-engine-foundation-tradeoffs.md)).
4. 요청마다 snapshot isolation 충돌 검사(같은 키를 스냅샷 이후 다른 트랜잭션이 썼는가)를 하고, 통과하면 커밋 타임스탬프를 받아 트리에 반영한다. 페이지 분할도 여기서 일어난다. 실패한 요청은 abort로 응답한다.
5. 이번 묶음에서 바뀐 페이지들의 이미지를 메모리에 복사해 두고, PAGE 레코드들 + COMMIT 레코드를 WAL에 쓴다.
6. WAL을 fsync한다. Rust `File::sync_data()`는 Linux에서 `fdatasync`, macOS에서 `fcntl(F_FULLFSYNC)`를 호출한다(10.9 참조).
7. 성공하면 durable 커밋 타임스탬프를 올리고(이제 새 스냅샷이 이 묶음을 본다), 묶음의 모든 클라이언트에 "커밋됨"을 응답한다.

[추론] 반영(4단계)을 스레드 하나가 하므로, 커밋 묶음의 경계가 항상 "트랜잭션 전체" 단위가 된다. 한 트랜잭션의 절반만 들어간 페이지 이미지가 생길 수 없다. 대신 이 스레드는 메모리 작업만 하므로 짧다. 실행 시간 전체에 쓰기 잠금을 잡는 SQLite와 달리, Oakdb는 **트랜잭션 실행은 병렬이고 커밋 반영만 줄을 선다.** 이것이 ADR-0005 "동시 쓰기"의 의미와 맞는지는 ADR로 확인해야 한다 (11절 참조).

[추론] 5단계 직후 크래시가 나서 COMMIT은 디스크에 있지만 응답은 못 했다면, 재시작 뒤 그 트랜잭션은 살아 있다. "응답 안 한 커밋은 살 수도 죽을 수도 있다"는 것은 허용된다. 반대로 응답한 커밋은 반드시 살아 있어야 한다.

### 10.6 체크포인트

조건: WAL이 일정 크기(예: 64 MiB)를 넘거나 dirty 페이지가 일정 수를 넘으면.

1. 커밋 담당 스레드를 잠깐 멈춘다(지금 묶음의 fsync까지 끝낸 뒤).
2. 지금 dirty 페이지 이미지를 메모리에 복사하고, `ckpt_lsn = 마지막 COMMIT의 LSN`을 기억한다. 멈춤 해제. (이 페이지들의 이미지는 모두 이미 fsync된 WAL에 있다. → WAL 규칙 충족)
3. 복사본을 `oak.db` 제자리에 쓴다. 필요하면 파일을 늘린다.
4. `oak.db`를 fsync한다.
5. 더 오래된 메타 슬롯에 `meta_seq+1`, `checkpoint_lsn = ckpt_lsn` 등을 쓰고 `oak.db`를 다시 fsync한다.
6. `ckpt_lsn`보다 앞만 담은 WAL 세그먼트를 지우고 디렉터리를 fsync한다.

[추론] 3단계 도중 페이지가 찢어져도 괜찮다. 5단계의 메타가 아직 안 써졌으므로 재시작하면 **이전** 체크포인트 LSN부터 redo하고, 찢어진 페이지는 WAL 안의 전체 이미지로 덮어써진다. 이 페이지는 이전 체크포인트 뒤에 바뀐 페이지이므로 그 이미지가 반드시 WAL에 있다. PostgreSQL FPW의 "체크포인트 뒤 첫 변경"과 같은 논리다. 5단계에서 메타 슬롯이 찢어져도 다른 슬롯이 멀쩡하다.

[추론] 1~2단계의 멈춤은 메모리 복사 시간뿐이다. ARIES의 fuzzy checkpoint보다 단순하지만 블로킹 체크포인트처럼 디스크 쓰기 동안 멈추지는 않는다.

### 10.7 복구 절차 (재시작할 때)

1. `oak.db`의 메타 슬롯 두 개를 읽어 CRC가 맞는 것 중 `meta_seq`가 큰 쪽을 고른다. 둘 다 깨졌으면 열기를 거부한다.
2. `checkpoint_lsn`이 들어 있는 세그먼트부터 WAL을 앞으로 읽는다. 레코드마다 길이, CRC, LSN 연속성을 검사한다.
3. PAGE 레코드는 임시 목록에 모은다. COMMIT 레코드를 만나면 모아 둔 이미지를 `oak.db`(또는 버퍼 풀)에 순서대로 덮어쓰고, `page_count`, `max_commit_ts`를 갱신한다.
4. 처음으로 검사에 실패한 레코드에서 멈춘다. 그 뒤 COMMIT 없이 남은 PAGE들은 버린다. 이것은 응답하지 않은 묶음이다.
   - 실패한 레코드 **뒤에** 정상 세그먼트가 더 있으면 꼬리가 아니라 중간 손상이다. 조용히 넘어가지 말고 열기를 거부한다 (RocksDB `kAbsoluteConsistency`에 가까운 태도, [0001 2절](0001-engine-foundation-tradeoffs.md)).
5. `oak.db`를 fsync하고, 체크포인트(10.6의 5~6단계)를 한 번 해서 WAL을 정리한다. 마지막 세그먼트의 깨진 꼬리는 잘라 내거나 새 세그먼트로 시작한다.
6. 타임스탬프 발급기를 `max_commit_ts + 1`부터 시작한다.

[추론] 전체 페이지를 덮어쓰는 redo는 몇 번을 다시 해도 결과가 같다(멱등). 그래서 복구 도중 또 정전이 나도 1단계부터 다시 하면 된다. ARIES의 pageLSN 비교나 CLR이 필요 없다.

### 10.8 torn page 전략 정리

| 어디가 찢어지나 | 결과 | 대책 |
|---|---|---|
| WAL 꼬리 레코드 | CRC 불일치 | 거기서 멈춤. 응답 안 한 묶음이라 손실 아님 |
| 체크포인트 중 `oak.db` 데이터 페이지 | 페이지 CRC 불일치 | 이전 체크포인트부터 redo하면 WAL 이미지로 덮어써짐 |
| 메타 슬롯 | 메타 CRC 불일치 | 다른 슬롯 사용 |
| 체크포인트 밖에서 `oak.db` 페이지 손상(비트 썩음 등) | 읽을 때 페이지 CRC 불일치 | 고칠 사본이 없음 → 오류 반환, 엔진 읽기 전용 전환. 숨기지 않는다 |

### 10.9 fsync 실패 정책

- WAL fsync 실패: 그 묶음의 클라이언트에는 abort가 아니라 **"결과 불명"** 오류를 준다(디스크에 갔는지 알 수 없다). 엔진은 **poisoned** 상태가 되어 새 쓰기를 모두 거부한다. 재시도하지 않는다. 재시작하면 10.7로 복구한다 ([0001 4절](0001-engine-foundation-tradeoffs.md), PostgreSQL `data_sync_retry=off`와 같은 태도).
- 체크포인트 중 `oak.db` fsync 실패: 메타를 쓰지 않고 poisoned. WAL이 아직 남아 있으므로 재시작 뒤 redo로 복구된다.
- 짧은 쓰기, `ENOSPC`도 fsync 실패와 같게 다룬다.
- [사실] Rust 표준 라이브러리의 `File::sync_all()`과 `File::sync_data()`는 Apple 플랫폼에서 `fcntl(fd, F_FULLFSYNC)`를 호출한다. Linux에서는 각각 `fsync`, `fdatasync`다 ([rust-lang/rust library/std/src/sys/fs/unix.rs](https://github.com/rust-lang/rust/blob/master/library/std/src/sys/fs/unix.rs)). 0001에서 [미검증]으로 남긴 항목이다. 그래도 sync 호출은 `Fs` trait 한곳에만 둔다(ADR-0003).

### 10.10 검증 계획 (실험 문서용 메모)

0001 6절의 `Fs` 시뮬레이터로 다음 장애를 seed 기반으로 주입한다.

- WAL 쓰기 도중 크래시(마지막 레코드 일부만 남음)
- 체크포인트의 데이터 페이지 쓰기 도중 크래시 + 그 페이지를 앞 절반만 새 내용으로 남기기(torn)
- 메타 슬롯 쓰기 도중 크래시
- 복구 도중 다시 크래시
- WAL fsync 실패 주입 → 엔진이 멈추는지, 재시작 뒤 "응답한 커밋은 모두 있음, 응답 안 한 커밋은 통째로 있거나 통째로 없음"이 성립하는지 검사

### 10.11 이번 설계에서 일부러 뺀 것 (나중에 할 일)

- dirty 페이지를 체크포인트 전에 데이터 파일로 내보내기(eviction). v1은 dirty 페이지가 메모리에 다 들어간다고 가정하고, 넘치면 체크포인트를 앞당긴다.
- 빈 페이지 재사용. v1은 파일이 늘기만 한다.
- 로그 양 줄이기(선택지 E): 페이지 LSN을 비교하는 redo로 바꾸고, 체크포인트 뒤 첫 변경만 전체 이미지를 쓴다.
- 오래된 MVCC 버전 정리(GC). GC도 트리를 바꾸므로 같은 커밋 경로(시스템 트랜잭션)로 로그를 남긴다.

---

## 11. ADR로 남길 후보

- 복구 방식: "페이지 이미지 redo 전용 WAL + 커밋 때 반영(deferred update) + 메타 슬롯 두 개" (이 문서 10절)
- ADR-0005 해석: "동시 쓰기" = 트랜잭션 실행은 병렬, 커밋 반영은 한 스레드에서 짧게 직렬. B+Tree 페이지 분할이 커밋 스레드에서만 일어나므로, 쓰기끼리의 latch crabbing이 필요 없어지고 읽기와 커밋 스레드 사이의 latch만 남는다 [추론]. 동시성 조사(별도 이슈)에서 이것이 차별점 주장으로 충분한지 따져 봐야 한다.

---

## 미검증

- ARIES 원 논문 본문(스캔 PDF)을 직접 읽지 못했다. ARIES의 세부 내용은 논문 초록과 CMU 15-445 강의 노트로 확인했다. 논문이 torn page와 media recovery를 어떻게 다루는지는 확인하지 못했다.
- 4096 바이트 페이지 쓰기가 요즘 SSD(NVMe)에서 실제로 원자적인지는 1차 출처로 확인하지 않았다. 이 설계는 원자적이지 않다고 가정하므로 결과에 영향은 없다.
- 페이지 이미지 WAL의 로그 양이 Oakdb 작업 부하에서 문제가 될 만큼 큰지는 벤치마크 전까지 알 수 없다.
- WiredTiger가 체크포인트 블록을 정확히 어떻게 재사용하는지(copy-on-write 세부)는 공식 문서에서 "체크포인트끼리 블록을 공유한다"까지만 확인했다.
- PostgreSQL에서 abort된 트랜잭션의 행 버전이 undo 없이 "보이지 않는 죽은 행"으로 남는다는 설명은 이번 조사에서 1차 출처로 확인하지 않아 본문에 넣지 않았다.
- "트랜잭션 실행 병렬 + 커밋 반영 직렬"이 SQLite 대비 쓰기 처리량을 실제로 얼마나 올리는지는 [추론]이며 벤치마크로 확인해야 한다.

---

## 출처

**SQLite**
- Atomic Commit In SQLite: https://www.sqlite.org/atomiccommit.html
- Write-Ahead Logging: https://www.sqlite.org/wal.html
- Powersafe Overwrite: https://www.sqlite.org/psow.html
- Database File Format (롤백 저널 §3, WAL §4): https://www.sqlite.org/fileformat2.html
- PRAGMA (synchronous, wal_checkpoint): https://www.sqlite.org/pragma.html

**PostgreSQL** (18 문서 기준)
- WAL Introduction: https://www.postgresql.org/docs/current/wal-intro.html
- WAL Reliability: https://www.postgresql.org/docs/current/wal-reliability.html
- WAL Configuration (체크포인트): https://www.postgresql.org/docs/current/wal-configuration.html
- WAL settings (full_page_writes, checkpoint_timeout, max_wal_size): https://www.postgresql.org/docs/current/runtime-config-wal.html
- Data Checksums: https://www.postgresql.org/docs/current/checksums.html
- Database Page Layout: https://www.postgresql.org/docs/current/storage-page-layout.html

**ARIES**
- Mohan et al. *ARIES: A Transaction Recovery Method Supporting Fine-Granularity Locking and Partial Rollbacks Using Write-Ahead Logging*. ACM TODS 17(1), 1992. https://dsf.berkeley.edu/cs262/Aries.pdf
- 초록: https://www.cs.brandeis.edu/~liuba/abstracts/mohan.html
- CMU 15-445 Fall 2019 Lecture #20 Logging Schemes: https://15445.courses.cs.cmu.edu/fall2019/notes/20-logging.pdf
- CMU 15-445 Fall 2019 Lecture #21 ARIES Database Crash Recovery Algorithms: https://15445.courses.cs.cmu.edu/fall2019/notes/21-recovery.pdf

**MySQL InnoDB** (8.4 문서)
- Doublewrite Buffer: https://dev.mysql.com/doc/refman/8.4/en/innodb-doublewrite-buffer.html
- Redo Log: https://dev.mysql.com/doc/refman/8.4/en/innodb-redo-log.html

**LMDB / WiredTiger**
- Symas LMDB: https://www.symas.com/mdb
- Howard Chu. *MDB: A Memory-Mapped Database and Backend for OpenLDAP*. LDAPCon 2011: https://mail.openldap.org/pub/hyc/mdb-paper.pdf
- lmdb.h: https://github.com/LMDB/lmdb/blob/mdb.master/libraries/liblmdb/lmdb.h
- mdb.c (커밋 순서): https://github.com/LMDB/lmdb/blob/mdb.master/libraries/liblmdb/mdb.c
- WiredTiger Checkpoint (2.8.0): https://source.wiredtiger.com/2.8.0/checkpoint.html
- WiredTiger architecture, Checkpoint: https://source.wiredtiger.com/develop/arch-checkpoint.html
- MongoDB Journaling: https://www.mongodb.com/docs/v8.0/core/journaling/

**Rust**
- Rust 표준 라이브러리 `File::sync_all`/`sync_data` 구현: https://github.com/rust-lang/rust/blob/master/library/std/src/sys/fs/unix.rs

**이전 조사**
- 0001 엔진 기반 설계 트레이드오프 (fsync, fsyncgate, F_FULLFSYNC, Fs trait, crash 테스트): [0001-engine-foundation-tradeoffs.md](0001-engine-foundation-tradeoffs.md)

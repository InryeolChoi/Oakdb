# 0003. B+Tree에서 여러 쓰기를 동시에 허용하는 방법 조사

- 작성일: 2026-10-12
- 관련 이슈: #6
- 질문: B+Tree에서 여러 트랜잭션이 동시에 쓰게 하는 방법(래치 크래빙, B-link tree, 낙관적 잠금, 행 잠금 vs MVCC)은 각각 어떻게 동작하나? 2개월짜리 Rust 프로젝트에서는 무엇이 현실적인가? PostgreSQL, InnoDB, WiredTiger, LMDB, SQLite `BEGIN CONCURRENT`는 실제로 무엇을 쓰나?
- 이미 정해진 것: Rust(ADR-0001), B+Tree 키-값 엔진 위에 단순한 관계형 계층(ADR-0004), 차별점은 동시 쓰기와 네트워크 서버(ADR-0005), 격리 수준은 스냅샷 격리(같은 데이터를 나중에 쓴 쪽이 실패), 내구성은 group commit.
- 표기 규칙 (0001, 0002와 같음)
  - **[사실]**: 링크한 1차 출처(논문, 공식 문서, 소스 코드)에서 직접 확인한 내용
  - **[추론]**: 사실을 바탕으로 한 해석이나 의견
  - **[권고]**: 의견. 최종 결정은 ADR로 남긴다
  - **[미검증]**: 1차 출처로 직접 확인하지 못한 내용

---

## 한눈에 보기

이 문제는 **두 층**으로 나눠서 봐야 헷갈리지 않는다.

| 층 | 무엇을 지키나 | 보호 수단 | 얼마나 오래 잡나 | 대표 기법 |
|---|---|---|---|---|
| (1) 트리 안의 물리적 동시성 | 페이지(노드)가 반쯤 고쳐진 상태를 다른 스레드가 보지 않게 | **래치(latch)** | 마이크로초 (페이지 하나 고치는 동안) | 래치 크래빙, B-link, 낙관적 잠금 결합(OLC), copy-on-write |
| (2) 트랜잭션 수준 동시성 | 트랜잭션이 스냅샷 격리 규칙대로 보이고 충돌하게 | **락(lock)** 또는 **버전(MVCC)** | 트랜잭션이 끝날 때까지 | 행 잠금, MVCC(버전을 어디에 두나), 먼저 커밋한 쪽 승리 vs 먼저 고친 쪽 승리 |

- **래치**: 자료구조(페이지)를 짧게 보호하는 스레드 수준 자물쇠. Rust의 `Mutex`/`RwLock`이 이것이다.
- **락**: 트랜잭션이 끝날 때까지 "이 행은 내 것"이라고 표시하는 논리적 자물쇠. 롤백과 엮인다.
- [사실] 이 구분(락은 트랜잭션 수준·논리적·롤백 필요, 래치는 스레드 수준·자료구조 보호·짧게 보유)은 CMU 15-445 강의 노트의 정의를 따른다 (0001 문서 2절에 인용한 [CMU 15-445 Lecture #08 notes](https://15445.courses.cs.cmu.edu/fall2021/notes/08-indexconcurrency.pdf)).
- [사실] OLC 논문도 각주에서 "lock"이라는 말을 데이터베이스 문헌의 "latch" 뜻으로 쓴다고 밝힌다 ([Leis et al. 2019](http://sites.computer.org/debull/A19mar/p73.pdf), 1쪽 각주 1).

**핵심 관찰** [추론]: SQLite가 "쓰기는 한 번에 하나"인 이유는 (1)층이 아니라 (2)층이다. 한 쓰기 트랜잭션이 **시작부터 커밋까지** DB 전체 자물쇠를 잡는다. 그래서 Oakdb가 SQLite를 이기려면 (2)층에서 트랜잭션끼리 줄을 서지 않게 하는 것이 먼저이고, (1)층은 마이크로초 단위라 처음엔 단순하게 둬도 된다.

| 시스템 | (1) 트리 안 | (2) 트랜잭션 | 같은 데이터 충돌 시 |
|---|---|---|---|
| LMDB | copy-on-write, 쓰기 트랜잭션은 한 번에 하나 | 루트 단위 다중 버전 | 애초에 동시에 쓰는 트랜잭션이 없다 |
| PostgreSQL | Lehman–Yao B-link tree (+페이지 읽기 래치) | 힙(heap)에 행 버전, 행 잠금 | 먼저 고친 쪽 승리. 나중 쪽은 기다렸다가 오류 |
| InnoDB | (이번 조사에서 확인 못 함) | 클러스터드 인덱스는 제자리 수정 + undo 로그, 행 잠금 | REPEATABLE READ에서도 오류 없이 최신 커밋 행을 고친다 |
| WiredTiger | (세부 미확인) 원자적 연산, generation 기반 메모리 회수 | 키마다 메모리 안 update chain + history store | 충돌하면 즉시 `WT_ROLLBACK` |
| SQLite `BEGIN CONCURRENT` | 페이지 B-tree, 커밋은 직렬 | 낙관적, 커밋 때 검사 | 읽은 **페이지**가 바뀌었으면 커밋 실패 |

---

## 1층: 트리 안의 물리적 동시성 (래치)

**비유**: 도서관 서가(B+Tree)를 여러 사서가 동시에 정리한다. 책장 한 칸(페이지)이 꽉 차면 칸을 둘로 나누고(split), 위층 안내판(부모 노드)에 "이 칸 다음은 저 칸"이라고 고쳐 써야 한다. 안내판을 고치는 도중에 다른 사서가 반쯤 고친 안내판을 보면 엉뚱한 칸으로 간다. 래치는 "지금 이 칸은 내가 정리 중"이라는 팻말이다.

- **split(분할)**: 꽉 찬 페이지를 둘로 나누고 부모에 새 페이지를 가리키는 키를 넣는 일. 동시성 설계가 어려운 이유의 대부분이 이것이다.
- **safe(안전한) 노드**: 이번 삽입으로 split되지 않을 만큼 여유가 있는 노드.

### 1-A. 래치 크래빙 (latch crabbing / lock coupling)

**비유**: 게가 걷듯, 다음 칸 팻말을 먼저 잡고 나서야 이전 칸 팻말을 놓는다.

- [사실] 1977년에 제안된 전통적 방법이고 "latch coupling", "hand-over-hand locking", "crabbing"이라고도 부른다. 트리를 내려가는 동안 최대 두 개의 락만 동시에 잡는다 ([Leis et al. 2019](http://sites.computer.org/debull/A19mar/p73.pdf), 2절).
- [사실] 부모 래치를 잡고, 자식 래치를 잡은 뒤, 자식이 safe면 부모 래치를 놓는다. 개선판은 잎(leaf)까지 읽기 래치로 내려가고 잎만 쓰기 래치로 잡으며, 잎이 unsafe면 처음부터 쓰기 래치로 다시 내려간다. 위에서 아래로만 잡으면 교착(deadlock)이 생기지 않지만, 잎을 양방향으로 훑으면 교착 위험이 있다 (0001에 인용한 [CMU notes](https://15445.courses.cs.cmu.edu/fall2021/notes/08-indexconcurrency.pdf)).
- [사실] 단점: 모든 연산이 루트 노드의 래치를 잡아야 한다. 읽기 모드 래치라도 잡고 놓을 때 공유 메모리에 쓰기가 일어나서, 다중 코어 CPU에서는 루트 래치의 캐시 라인이 병목이 된다. 논문은 "the lock of the root node of a B-tree" 같은 자주 잡히는 락이 확장성을 크게 제한한다고 쓴다 ([Leis et al. 2019](http://sites.computer.org/debull/A19mar/p73.pdf), 1절, 3절).
- [추론] 디스크 기반이고 코어 수가 많지 않은 Oakdb에서는 루트 캐시 경합보다 fsync와 네트워크 지연이 훨씬 크다. 크래빙의 확장성 한계는 Oakdb 벤치마크에서 먼저 드러날 문제가 아니다.

### 1-B. B-link tree (Lehman & Yao 1981)

**비유**: 칸마다 "내 다음 칸은 오른쪽 저기"라는 화살표(오른쪽 링크)와 "이 칸에는 ○○까지만 있음"이라는 꼬리표(high key)를 붙인다. 안내판 고치는 게 늦어져도, 찾는 책이 꼬리표보다 뒤면 화살표를 따라 오른쪽으로 가면 된다.

- [사실] Lehman과 Yao는 각 노드에 링크 포인터 하나를 더해서, 다른 프로세스가 트리를 고치는 중이어도 쉽게 회복하게 했다. 읽기 락을 쓰지 않고, 갱신하는 프로세스는 작은 상수 개수의 노드만 잠근다 ([Lehman & Yao 1981](https://www.csd.uoc.gr/~hy460/pdf/p650-lehman.pdf), 초록). 동시에 잠그는 노드는 최대 3개다 (같은 논문 본문).
- [사실] 노드가 split되면 링크 포인터가 split과 동시에 생겨 "임시 땜질" 역할을 한다. 탐색 중 찾는 키가 노드의 high key보다 크면 트리 구조가 바뀐 것이므로 링크 포인터로 옆 노드에 간다 (같은 논문 3절).
- [사실] 삭제는 단순하게 처리한다. 잎 노드가 k개보다 적은 항목을 갖는 것을 허용하고(병합 안 함), 저장 효율이 너무 떨어지면 일괄 재구성이나 트리 전체를 잠그는 underflow 연산을 한다 (같은 논문 7절).
- [사실] Leis et al.은 B-link가 부모 래치를 일찍 놓을 수 있어 자식을 디스크에서 읽어야 할 때 유리하지만, 메모리 안 작업에서는 크래빙만큼 락을 잡으므로 같은 확장성 문제가 있다고 평가한다. 또 B-link 프로토콜은 노드 병합을 지원하지 않는다고 쓴다 ([Leis et al. 2019](http://sites.computer.org/debull/A19mar/p73.pdf), 2절).

**PostgreSQL이 실제로 하는 것**
- [사실] PostgreSQL B-tree 인덱스(nbtree)는 Lehman & Yao 알고리즘을 쓰고, 삭제는 Lanin & Shasha의 단순화판을 쓴다. 각 페이지에 오른쪽 링크와 high key를 둔다 ([nbtree README](https://github.com/postgres/postgres/blob/master/src/backend/access/nbtree/README)).
- [사실] 원 논문은 읽기 락이 필요 없다고 하지만 그 전제는 "메모리 안 페이지 사본이 공유되지 않는다"는 것이다. PostgreSQL은 버퍼를 여러 백엔드가 공유하므로 btree 페이지에 페이지 단위 읽기 락을 건다. README는 "This reduces concurrency but guarantees correct behavior"라고 쓴다 (같은 README).
- [사실] split 후 부모에 키를 넣을 때는 자식 페이지를 부모 삽입이 끝날 때까지 잠가 두며, 트리를 올라갈 때 부모와 자식 락을 결합하는 보수적인 방식을 "far simpler"라서 유지한다 (같은 README).
- [추론] 즉 "논문 그대로의 B-link"는 실제 시스템에서도 읽기 래치, 역방향 스캔용 왼쪽 링크, 삭제 처리 등 보완이 많이 붙는다. 2개월 안에 손으로 맞추기에는 함정이 많다.

### 1-C. 낙관적 잠금 결합 (Optimistic Lock Coupling, OLC)

**비유**: 칸을 볼 때 팻말을 걸지 않고, 칸에 붙은 "수정 횟수 카운터"만 보고 읽는다. 다 읽은 뒤 카운터가 그대로면 OK, 바뀌었으면 처음부터 다시 읽는다.

- [사실] 각 락에 수정할 때마다 올라가는 버전 카운터를 붙인다. 읽기는 (1) 버전 읽기 (2) 노드 읽기 (3) 버전이 그대로인지 확인, 실패하면 재시작한다. 쓰기는 보통 락처럼 잡고, 고치고, 버전을 올리고 푼다 ([Leis et al. 2019](http://sites.computer.org/debull/A19mar/p73.pdf), 3.1절).
- [사실] 저자들은 OLC가 확장성이 높고 전통적인 lock coupling만큼 구현이 쉽다고 주장한다. 경합이 심하면 비관적 락으로 되돌아갈 수 있다는 점이 lock-free 방식보다 견고한 장점이라고 쓴다 (같은 논문 초록, 3.4절).
- [사실] 주의점 두 가지를 논문이 직접 적는다. (1) 다른 스레드가 낙관적으로 읽고 있을 수 있어 노드를 바로 해제할 수 없으므로 epoch 기반 회수나 hazard pointer가 필요하다. (2) 쓰기 도중의 노드를 읽다가 무한 루프에 빠지지 않도록 노드 안 이진 탐색 등을 항상 끝나게 짜야 한다 (같은 논문 3.3–3.4절).
- [사실] 논문의 평가 대상은 메모리 안 B-tree다 (같은 논문 1절 요약).
- [사실] Rust에서 데이터 경합(data race)은 정의되지 않은 동작(UB)이다 ([Rust Reference: Behavior considered undefined](https://doc.rust-lang.org/reference/behavior-considered-undefined.html)).
- [추론] OLC의 "고쳐지는 중일 수도 있는 메모리를 일단 읽고 나중에 검증"하는 패턴은 Rust에서 그대로 쓰면 UB가 된다. 페이지 바이트를 원자적 연산으로 읽거나 `unsafe`로 감싸는 방식을 따로 설계해야 한다. 거기에 버퍼 풀의 페이지 교체(eviction)와 메모리 회수까지 맞물려서, 2개월·AI 작성 코드·초보 리뷰라는 조건에서는 위험이 크다.

### 1-D. Copy-on-write B+Tree (LMDB)

**비유**: 책장 칸을 고치지 않고, 바뀐 칸과 그 위 안내판들을 **새로 만들어** 옆에 둔다. 다 만들면 맨 위 "현관 안내판(루트)"만 새 것으로 바꿔 건다. 옛 안내판을 보던 사람은 끝까지 옛 서가를 본다.

- [사실] LMDB는 데이터 페이지에 copy-on-write를 써서 활성 페이지를 절대 덮어쓰지 않는다. 그래서 손상에 강하고 크래시 후 특별한 복구 절차가 필요 없다 ([lmdb.h 소개 주석](https://github.com/LMDB/lmdb/blob/mdb.master/libraries/liblmdb/lmdb.h)).
- [사실] "Writes are fully serialized; only one write transaction may be active at a time". 그 덕에 writer끼리 교착이 없다. 구조가 다중 버전이라 reader는 락 없이 돌고, writer와 reader는 서로를 막지 않는다 (같은 주석).
- [사실] LMDB는 WAL이나 append-only 쓰기를 쓰지 않고 파일 안에서 빈 페이지를 추적해 재사용한다. 단, 오래 열린 읽기 트랜잭션은 새 쓰기가 비운 페이지의 재사용을 막아 DB가 빠르게 커질 수 있다 (같은 주석, Caveats).
- [추론] COW에서는 트리 **안**에 래치가 거의 필요 없다. reader는 자기가 시작할 때의 루트만 따라가고, writer는 하나뿐이기 때문이다. 대신 "writer 하나"가 그대로 남는다. 이것을 Oakdb에서 쓰려면 2층에서 "트랜잭션 실행은 병렬, 트리 반영만 한 줄로"를 따로 만들어야 한다 (아래 선택지 B).
- [추론] ADR-0004는 "B+Tree를 제자리에서 수정"한다는 전제로 결과를 적었다. COW를 고르면 그 전제와 복구 설계(WAL 대신 루트 교체)가 바뀌므로 새 ADR이 필요하다.

### 1층 정리

| 기법 | 동시에 잡는 래치 | 2개월 난이도 | Oakdb 메모 |
|---|---|---|---|
| 트리 전체 `RwLock` 하나 | 1개 (트리 전체) | 하 | 반영 구간이 마이크로초라면 출발점으로 충분 [추론] |
| 래치 크래빙 | 최대 2개 | 중 | 교과서적, 테스트·리뷰가 쉬움 |
| B-link | 최대 3개(쓰기), 읽기 락 없음(원 논문) | 상 | 실제 구현(PG)에 보완이 많음 |
| OLC | 쓰기만 잡음 | 상~최상 | Rust UB·메모리 회수·eviction 문제 |
| COW (LMDB) | 거의 없음, writer 하나 | 중 | 2층에서 병렬 실행을 따로 만들어야 함 |

---

## 2층: 트랜잭션 수준 동시성 (스냅샷 격리)

**비유**: 공유 문서를 여러 사람이 고친다. 스냅샷 격리는 "각자 문서를 연 순간의 사본을 보고 작업하고, 저장할 때 **같은 문장을 다른 사람이 먼저 저장했으면** 내 저장이 거절된다"는 규칙이다.

### 스냅샷 격리(SI)의 정의
- [사실] 각 트랜잭션은 시작 시점(Start-Timestamp)의 커밋된 데이터 스냅샷을 읽는다. 스냅샷을 유지할 수 있는 한 읽기는 막히지 않는다. 커밋할 때 Commit-Timestamp를 받고, 자기 실행 구간 [Start, Commit] 안에 커밋한 다른 트랜잭션이 **자기가 쓴 데이터를 썼다면** abort한다. 이것을 "First-committer-wins"라고 부르며 lost update를 막는다 ([Berenson et al. 1995](https://www.microsoft.com/en-us/research/wp-content/uploads/2016/02/tr-95-51.pdf), 4.2절).
- [사실] SI는 직렬화 가능(serializable)하지 않다. 대표 이상 현상이 write skew(A5B)다. 두 트랜잭션이 x, y를 읽고 하나는 x, 하나는 y를 쓰면 둘 다 커밋되어 x와 y 사이 제약이 깨질 수 있다 (같은 논문 4.2절).
- [추론] SI의 충돌 검사는 **쓰기 집합끼리만** 비교하면 된다. 0002 문서는 "key 단위 read/write set 비교"라고 적었는데, 읽기 집합까지 비교하면 SI보다 엄격한 낙관적 직렬화 검사가 되어 abort가 늘어난다. ADR에서 "SI = 쓰기-쓰기 충돌만 검사"로 명확히 하는 것이 좋다.
- **write skew**: 서로 다른 데이터를 고쳤는데 합쳐 보면 규칙이 깨지는 현상. 예: "당직 의사는 최소 1명" 규칙에서 두 의사가 동시에 "다른 사람이 있으니 나는 빠진다"를 커밋.

### 2-A. 행 잠금 (row lock)

**비유**: 고치려는 문장에 내 이름표를 붙인다. 다른 사람은 그 문장을 고치려면 이름표가 떨어질 때까지 기다린다.

- [사실] InnoDB는 공유(S)/배타(X) 행 수준 잠금을 쓰고, record lock은 "a lock on an index record"다. REPEATABLE READ에서는 next-key lock(레코드 + 그 앞 간격)으로 팬텀 행을 막는다 ([MySQL 8.4: InnoDB Locking](https://dev.mysql.com/doc/refman/8.4/en/innodb-locking.html)).
- [사실] PostgreSQL에서 `DELETE`와 일부 `UPDATE`는 행에 `FOR UPDATE` 잠금을 잡는다. 행 잠금은 조회를 막지 않고 같은 행의 writer와 locker만 막는다. 또 수정된 행 정보를 메모리에 기억하지 않으므로 동시에 잠글 수 있는 행 수에 한계가 없다(대신 잠금이 디스크 쓰기를 일으킬 수 있다) ([PostgreSQL: Explicit Locking](https://www.postgresql.org/docs/current/explicit-locking.html)).
- [추론] 행 잠금은 "기다림"을 만든다. 기다림이 있으면 교착 감지(대기 그래프)나 타임아웃이 필요하다. 2개월 범위에서는 큰 짐이다. 기다리지 않고 바로 실패시키는(no-wait) 방식이면 락 테이블만 있으면 된다.

### 2-B. MVCC: 버전을 어디에 두나

**비유**: 문장을 덮어쓰지 않고 "수정 이력"을 남긴다. 각자는 자기가 문서를 연 시점에 맞는 이력을 골라 읽는다. 이력을 **어디에** 적어 두느냐가 시스템마다 다르다.

| 위치 | 대표 | 동작 | 치르는 비용 |
|---|---|---|---|
| 테이블에 행 버전을 통째로 (tuple version) | PostgreSQL | 새 버전을 새 행으로 쓴다 | 죽은 버전 청소(VACUUM) |
| 제자리 수정 + undo 로그 | InnoDB | 최신 값만 트리에, 옛 값은 undo에서 재구성 | undo 관리, purge |
| 메모리 안 별도 버전 저장소 | WiredTiger | 키마다 update chain, 오래된 것은 history store | 메모리, 이동 로직 |
| 트리 안에 (key, 버전) 항목 | (여러 KV 엔진, 0001 참고) | 키 뒤에 커밋 시각을 붙여 정렬 | 옛 버전 GC, 키가 길어짐 |
| 트리 전체를 버전으로 (COW 루트) | LMDB | 스냅샷 = 옛 루트 | 오래된 reader가 페이지를 묶어 둠 |

- [사실] PostgreSQL 행 헤더에는 `t_xmin`(삽입 XID), `t_xmax`(삭제 XID), `t_ctid`(이 버전 또는 더 새 버전의 위치)가 있다 ([PostgreSQL: Page Layout](https://www.postgresql.org/docs/current/storage-page-layout.html)). `UPDATE`나 `DELETE`는 옛 버전을 바로 지우지 않고, 아무도 안 보게 된 버전은 `VACUUM`이 회수한다 ([PostgreSQL: Routine Vacuuming](https://www.postgresql.org/docs/current/routine-vacuuming.html)). 인덱스 항목에는 가시성 정보가 없고 힙 항목에만 있다 ([PostgreSQL: Index-Only Scans](https://www.postgresql.org/docs/current/indexes-index-only-scans.html)).
- [사실] InnoDB는 각 행에 숨은 필드 `DB_TRX_ID`(마지막으로 쓴 트랜잭션)와 `DB_ROLL_PTR`(undo 로그 레코드를 가리킴)을 붙인다. 클러스터드 인덱스 레코드는 제자리 수정되고, 옛 버전은 undo 로그로 재구성한다. 삭제된 행은 purge가 나중에 물리적으로 지우며, purge가 밀리면 테이블이 계속 커질 수 있다 ([MySQL 8.4: InnoDB Multi-Versioning](https://dev.mysql.com/doc/refman/8.4/en/innodb-multi-versioning.html)).
- [사실] WiredTiger는 기본으로 스냅샷 격리로 돌고, 메모리 안 갱신을 최신이 맨 앞인 단일 연결 리스트(update chain)로 둔다. 거기서 보이는 값이 없으면 history store를 찾는다 ([WiredTiger: Transactions architecture](https://source.wiredtiger.com/develop/arch-transaction.html)).

### 2-C. 같은 데이터를 두 트랜잭션이 고칠 때: 누가 이기나

| 규칙 | 언제 검사 | 대표 | 장단점 [추론] |
|---|---|---|---|
| 먼저 커밋한 쪽 승리 (first-committer-wins) | 커밋 때 | Berenson et al.의 SI 정의, SQLite `BEGIN CONCURRENT`(페이지 단위) | 실행 중 기다림이 없다. 대신 다 해 놓고 마지막에 실패할 수 있다 |
| 먼저 고친 쪽 승리, 기다림 (first-updater-wins) | 쓰는 순간 | PostgreSQL REPEATABLE READ | 일찍 알 수 있지만 기다림·교착 처리가 필요 |
| 먼저 고친 쪽 승리, 즉시 실패 | 쓰는 순간 | WiredTiger | 기다림이 없고 일찍 실패. 미커밋 버전을 트리에 둬야 한다 |
| 실패 없음 (최신 커밋 값을 그냥 고침) | — | InnoDB REPEATABLE READ의 UPDATE | lost update 가능. Oakdb 결정과 맞지 않음 |

- [사실] PostgreSQL Repeatable Read는 스냅샷 격리로 구현된다. 고치려는 행을 동시 트랜잭션이 이미 고쳤으면 그 트랜잭션이 끝나길 기다린다. 그쪽이 롤백하면 진행하고, 커밋하면 "could not serialize access due to concurrent update"로 롤백된다 ([PostgreSQL: Transaction Isolation](https://www.postgresql.org/docs/current/transaction-iso.html)).
- [사실] WiredTiger는 낙관적 동시성 제어를 쓴다. "Writes do not block other writes"이고, 같은 값을 동시에 고치는 트랜잭션은 `WT_ROLLBACK`으로 실패하며, 애플리케이션이 롤백 후 재시도해야 한다 ([WiredTiger: Transactions API](https://source.wiredtiger.com/develop/transactions_api.html)).
- [사실] InnoDB 기본 REPEATABLE READ에서 스냅샷은 `SELECT`에만 적용되고, `UPDATE`/`DELETE`는 다른 트랜잭션이 방금 커밋한 행도 고칠 수 있다(문서는 이때 오류를 말하지 않는다) ([MySQL 8.4: Consistent Nonlocking Reads](https://dev.mysql.com/doc/refman/8.4/en/innodb-consistent-read.html)).
- [사실] SQLite `BEGIN CONCURRENT`는 실제 DB 잠금을 COMMIT 때까지 미루고, 커밋은 직렬화한다. 커밋 때 그 트랜잭션이 **읽은** 페이지가 시작 이후 바뀌었는지 검사해, 바뀌었으면 `SQLITE_BUSY_SNAPSHOT`을 돌려준다. 키가 가까우면(같은 페이지) 충돌한다. 그래서 연속 증가 rowid 대신 무작위 rowid를 쓰라고 권한다 ([begin_concurrent.md](https://www.sqlite.org/src/doc/begin-concurrent/doc/begin_concurrent.md)).
- [추론] SQLite 방식은 "읽은 페이지"까지 보므로 SI보다 엄격하고, 페이지 단위라 거짓 충돌이 생긴다. Oakdb가 "쓴 **키**끼리만" 비교하면 같은 페이지의 서로 다른 키를 고치는 트랜잭션은 둘 다 커밋된다. 0002에서 제안한 벤치마크(같은 페이지에 몰린 서로 다른 키)가 바로 이 차이를 보여 준다.

---

## Rust 관련 사실

- [사실] `parking_lot`(0.12.5)은 `Mutex`, `RwLock`, `Condvar`, `Once`를 제공하고, std보다 "smaller, faster and more flexible"하다고 소개한다 ([docs.rs](https://docs.rs/parking_lot/latest/parking_lot/)). `Mutex`는 1바이트, `RwLock`은 1워드 크기다. `RwLock`은 기다리는 writer에게 새 reader보다 우선권을 주는 task-fair 정책을 쓰고, "upgradable" 읽기 락을 쓰기 락으로 원자적으로 올릴 수 있다. 실험적 교착 감지기(`deadlock_detection` 기능)도 있다 ([parking_lot README](https://github.com/Amanieu/parking_lot)).
  - [추론] 페이지마다 래치를 두는 크래빙에서 래치 크기가 작다는 점, 그리고 "읽기로 내려가다 잎에서 쓰기로 올리기"에 upgradable 락이 직접 쓰인다.
- [사실] `crossbeam-epoch`(0.9.21)은 "Epoch-based memory reclamation"이다. 한 스레드가 원소를 제거해도 다른 스레드가 읽는 중일 수 있어 바로 해제할 수 없고, 스레드를 "pin"한 동안 제거된 객체의 해제를 미룬다 ([docs.rs](https://docs.rs/crossbeam-epoch/latest/crossbeam_epoch/)).
  - [추론] OLC나 lock-free 구조를 고를 때만 필요하다. COW 트리에서 "옛 루트를 보는 reader가 다 끝났나"를 판단할 때도 같은 아이디어(가장 오래된 활성 스냅샷 추적)가 필요하지만, 페이지 단위라 직접 만드는 편이 단순할 수 있다.
- [사실] `loom`(0.7.2)은 C11 메모리 모델이 허용하는 스레드 실행 순서를 바꿔 가며 테스트를 반복 실행한다. loom의 타입(원자 변수, 뮤텍스, `thread::spawn`)을 쓴 코드만 검사되고, `cfg(loom)`으로 바꿔 끼운다. 스레드 수가 늘면 경우의 수가 폭발해 `LOOM_MAX_PREEMPTIONS`로 제한하며, 2~3이면 대부분의 버그를 잡는다고 문서가 말한다. `Relaxed` 순서의 모든 재배치를 모델링하지는 못한다 ([docs.rs](https://docs.rs/loom/latest/loom/)).
  - [추론] 페이지 래치 프로토콜이나 커밋 큐 같은 **작은 핵심부**를 2~3 스레드로 검사하는 데 맞다. 트리 전체를 loom으로 돌리는 것은 현실적이지 않다. 트리 전체는 무작위 스트레스 테스트 + 불변식 검사로 보완한다.

---

## Oakdb 선택지

조건: 2개월, 코드는 대부분 AI가 작성하고 사람이 읽고 설명해야 함, 크래시 복구와 group commit도 같은 기간에 해야 함, 목표는 "SQLite의 writer 하나"를 이기는 것.

### 선택지 A. 병렬 실행 + 직렬 반영 + 트리 안 버전 + 트리 전체 래치에서 시작
- 트랜잭션은 시작 시각의 스냅샷을 읽고, 쓰기는 **자기만의 쓰기 버퍼**에 모은다(트리는 건드리지 않음).
- 커밋 때 커밋 큐에서 한 줄로: "내 시작 이후 커밋된 키 중 내가 쓴 키가 있나?"만 검사(first-committer-wins, 키 단위). 통과하면 `(key, commit_ts)` 항목으로 트리에 반영하고, 같은 배치의 트랜잭션들을 한 번의 fsync로 묶는다(group commit).
- 트리는 처음엔 `RwLock` 하나로 보호한다. 반영은 커밋 스레드 하나만 하므로 writer끼리 트리에서 경합하지 않는다.
- SQLite를 이기는 지점 [추론]: (1) 트랜잭션이 실행·네트워크 왕복 동안 아무 잠금도 잡지 않는다. (2) 충돌 판정이 페이지가 아니라 키. (3) fsync를 여러 트랜잭션이 나눠 낸다.
- 적합도: **상**. 난이도: **중하**. 위험: 트리 전체 래치 때문에 반영 중에는 읽기가 잠깐 멈춘다. 옛 버전 GC가 필요하다.

### 선택지 A+. A에서 래치 크래빙으로 업그레이드
- A가 동작한 뒤, 벤치마크에서 트리 래치가 병목으로 보이면 페이지별 `parking_lot::RwLock`으로 바꾼다. 개선판 크래빙(읽기로 내려가고 잎만 쓰기, unsafe하면 재시작)을 쓴다.
- 적합도: **상**(단계적). 난이도: **중**. "전역 래치 → 크래빙 전후 비교"가 그대로 블로그 한 편이 된다.

### 선택지 B. COW B+Tree(LMDB식) + 병렬 실행 + 직렬 반영
- A와 2층은 같고, 1층을 COW로 바꾼다. 스냅샷 = 시작 시각의 루트이므로 트리 안에 버전을 둘 필요가 없고 reader는 래치가 없다.
- 적합도: **상**. 난이도: **중**. 위험: ADR-0004의 "제자리 수정" 전제를 바꿔야 한다. 빈 페이지 추적·회수, 오래 열린 스냅샷이 페이지를 묶어 두는 문제(LMDB 경고와 같음), 커밋마다 루트까지 경로 복사(배치로 줄임). 0002의 2순위(싼 브랜치)와 바로 이어진다는 장점이 있다.

### 선택지 C. 제자리 B+Tree + 미커밋 버전을 트리에 바로 기록 + 쓰는 순간 충돌 검사(WiredTiger식 즉시 실패)
- 쓰기를 실행 중에 바로 트리에 넣으므로 여러 writer가 트리를 **진짜로 동시에** 고친다. 크래빙이 필수이고, 롤백 때 미커밋 버전을 지워야 한다.
- 적합도: **중**. 난이도: **상**. 일찍 실패를 알 수 있지만, 롤백·복구·래치가 한꺼번에 엮인다.

### 선택지 D. 행 잠금 + undo 로그 (InnoDB식)
- 적합도: **하**. 난이도: **상**. 락 매니저, 교착 감지, undo/purge, ARIES식 복구가 모두 필요하다. 또 InnoDB 기본 동작은 Oakdb가 정한 "나중 쓴 쪽 실패"와 다르다.

### 선택지 E. B-link tree 또는 OLC
- 적합도: **하**. 난이도: **상~최상**. 이득(많은 코어에서의 확장성, 디스크 읽기 중 부모 래치 해제)이 Oakdb 벤치마크에서 먼저 드러날 병목이 아니다 [추론]. "왜 고르지 않았나"를 ADR에 남기는 것으로 충분하다.

| 선택지 | 2개월 적합도 | SQLite 대비 차별점 | 난이도 |
|---|---|---|---|
| A. 병렬 실행 + 직렬 반영 + 트리 안 버전 + 전역 래치 | 상 | 충분 | 중하 |
| A+. A → 래치 크래빙 | 상 | 충분 + 읽기 동시성 | 중 |
| B. COW + 병렬 실행 + 직렬 반영 | 상 | 충분 + 브랜치로 확장 | 중 (ADR-0004 수정) |
| C. 미커밋 버전 트리 기록 + 즉시 실패 | 중 | 충분 | 상 |
| D. 행 잠금 + undo | 하 | 충분 | 상 |
| E. B-link / OLC | 하 | 차이 없음 | 상~최상 |

---

## [권고]

**선택지 A로 시작하고, 시간이 남으면 A+(래치 크래빙)로 올린다.**

1. [권고] 2층(트랜잭션)을 먼저 확정한다: 스냅샷 읽기 + 개인 쓰기 버퍼 + 커밋 때 **쓴 키끼리만** 비교하는 first-committer-wins + 커밋 큐에서 group commit. 이것만으로 SQLite의 "한 번에 writer 하나"와 `BEGIN CONCURRENT`의 "페이지 단위 충돌"을 둘 다 넘는다 [추론].
2. [권고] 1층(트리)은 트리 전체 `RwLock`으로 시작한다. 반영은 커밋 스레드 하나만 하므로 split 동시성 문제가 아직 생기지 않는다. 두 층 사이를 "트리는 `get/put/scan`만 안다, 버전·충돌은 트랜잭션 층이 안다"는 경계로 나눠 두면 1층을 나중에 바꿔도 2층 코드는 그대로다.
3. [권고] 크래빙은 벤치마크로 필요가 보일 때 넣는다. 넣을 때는 래치 프로토콜만 따로 떼어 `loom`으로 2~3 스레드 검사를 하고, 트리 전체는 스트레스 테스트로 검증한다.
4. [권고] 버전은 트리 안 `(key, commit_ts)` 항목으로 둔다. ADR-0004의 제자리 수정 전제를 그대로 쓰고, GC는 "가장 오래된 활성 스냅샷보다 오래된 버전 중 최신 하나만 남기기"로 단순하게 한다.
5. [권고] B(COW)는 크래시 복구 설계가 WAL + 제자리 수정으로 너무 무거워질 때의 대안으로 남긴다. 바꾸게 되면 ADR-0004를 수정하는 새 ADR을 쓴다.
6. [권고] ADR에 "Oakdb의 SI는 write skew를 허용한다"를 명시한다(Berenson et al.).

---

## 미검증

- InnoDB가 B-tree 페이지 안에서 쓰는 래치 방식(인덱스 단위 락, 페이지 래치 등)은 1차 출처로 확인하지 못했다. 표에서 "확인 못 함"으로 둔다.
- WiredTiger의 B-tree 페이지 접근 방식(hazard pointer, split 처리)은 확인한 문서 페이지([Concurrency](https://source.wiredtiger.com/develop/arch-concurrency.html))에 직접 나오지 않았다. generation 기반 회수와 원자적 연산 사용만 확인했다.
- InnoDB REPEATABLE READ의 동작은 MySQL 8.4 문서 기준이다. 이후 버전에서 바뀌었는지는 확인하지 않았다.
- "전역 래치로 시작해도 반영 구간이 마이크로초라 병목이 되지 않는다"는 판단은 [추론]이다. Oakdb 벤치마크로 확인해야 한다.
- Turso가 버전을 어디에 저장하는지(별도 메모리 저장소인지)는 이번에 1차 출처로 다시 확인하지 않았다.
- Lehman & Yao 원문은 대학 강의 사이트에 올라온 사본으로 읽었다(ACM TODS 6(4), 1981, pp. 650–670).

---

## 출처

**논문**
- P. L. Lehman, S. B. Yao. "Efficient Locking for Concurrent Operations on B-Trees." ACM TODS 6(4), 1981: https://www.csd.uoc.gr/~hy460/pdf/p650-lehman.pdf
- V. Leis, M. Haubenschild, T. Neumann. "Optimistic Lock Coupling: A Scalable and Efficient General-Purpose Synchronization Method." IEEE Data Engineering Bulletin 42(1), 2019: http://sites.computer.org/debull/A19mar/p73.pdf
- H. Berenson et al. "A Critique of ANSI SQL Isolation Levels." SIGMOD 1995 (MSR-TR-95-51): https://www.microsoft.com/en-us/research/wp-content/uploads/2016/02/tr-95-51.pdf
- CMU 15-445 Fall 2021 Lecture #08 notes (0001에서 인용): https://15445.courses.cs.cmu.edu/fall2021/notes/08-indexconcurrency.pdf

**PostgreSQL**
- nbtree README: https://github.com/postgres/postgres/blob/master/src/backend/access/nbtree/README
- Transaction Isolation: https://www.postgresql.org/docs/current/transaction-iso.html
- Explicit Locking: https://www.postgresql.org/docs/current/explicit-locking.html
- Database Page Layout: https://www.postgresql.org/docs/current/storage-page-layout.html
- Routine Vacuuming: https://www.postgresql.org/docs/current/routine-vacuuming.html
- Index-Only Scans: https://www.postgresql.org/docs/current/indexes-index-only-scans.html

**MySQL InnoDB (8.4)**
- Multi-Versioning: https://dev.mysql.com/doc/refman/8.4/en/innodb-multi-versioning.html
- Locking: https://dev.mysql.com/doc/refman/8.4/en/innodb-locking.html
- Consistent Nonlocking Reads: https://dev.mysql.com/doc/refman/8.4/en/innodb-consistent-read.html

**WiredTiger**
- Transactions (architecture): https://source.wiredtiger.com/develop/arch-transaction.html
- Transactions API: https://source.wiredtiger.com/develop/transactions_api.html
- Concurrency: https://source.wiredtiger.com/develop/arch-concurrency.html

**LMDB**
- lmdb.h (소개 주석, Caveats): https://github.com/LMDB/lmdb/blob/mdb.master/libraries/liblmdb/lmdb.h

**SQLite**
- BEGIN CONCURRENT: https://www.sqlite.org/src/doc/begin-concurrent/doc/begin_concurrent.md

**Rust**
- parking_lot docs: https://docs.rs/parking_lot/latest/parking_lot/
- parking_lot README: https://github.com/Amanieu/parking_lot
- crossbeam-epoch docs: https://docs.rs/crossbeam-epoch/latest/crossbeam_epoch/
- loom docs: https://docs.rs/loom/latest/loom/
- Rust Reference, Behavior considered undefined: https://doc.rust-lang.org/reference/behavior-considered-undefined.html

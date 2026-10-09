# 0001. 엔진 기반 설계 트레이드오프 조사

- 작성일: 2026-10-09
- 목적: Rust로 약 2개월 동안 만드는 학습·포트폴리오용 DB 엔진에서 아직 정하지 않은 6가지 기반 결정을 1차 출처 기준으로 정리한다.
- 범위 전제: 핵심은 **스토리지 엔진 + crash recovery**, **동시성 + 트랜잭션**이다. 쿼리 처리와 네트워크 서버는 최소로 두고, 분산은 뒤로 미룬다. 장기적으로 relational/document/vector 모델을 그 위에 얹는다.
- 표기 규칙
  - **[사실]**: 링크한 1차 출처(공식 문서, 소스, 논문, man page)에서 직접 확인한 내용
  - **[추론]**: 사실을 바탕으로 이 프로젝트에 적용한 해석
  - **[권고]**: 의견. 최종 결정은 ADR로 남긴다
  - **[미검증]**: 1차 출처로 확인하지 못한 내용

---

## 1. 기본 데이터 모델: ordered KV vs table/row-native

### 선택지
- **A. Ordered key-value** (bytes→bytes, 정렬 유지, range scan 제공). 행, 문서, 벡터는 key encoding으로 이 위에 매핑한다.
- **B. Table/row-native**: PostgreSQL heap이나 BusTub처럼 페이지 안에 tuple을 두고 엔진이 스키마와 tuple을 직접 다룬다.

### 사실
- [사실] FoundationDB는 "ordered key-value data model"을 쓰며, key가 사전순으로 정렬돼 있어 range read가 효율적이라고 설명한다. key와 value에는 정해진 형식이 없고 그냥 binary data다 ([FDB Features](https://apple.github.io/foundationdb/features.html)).
- [사실] FoundationDB는 "decouples its data storage technology from its data model"이고, core에는 indexing이 없으며 앞으로도 넣지 않는다고 밝힌다. index는 layer가 data용 KV와 index용 KV, 두 종류를 저장해서 구현한다 (예: `people/alice/eye_color = blue`와 `eye_color/blue/alice = true`). 여기에 ACID 트랜잭션이 더해지는 것이 핵심이다 ([FDB Layer Concept](https://apple.github.io/foundationdb/layer-concept.html)).
- [사실] FDB Record Layer는 "a record-oriented data store and a relational database interface built on top of the FoundationDB key/value store"다. SQL(JDBC) 인터페이스를 제공하고, 타입 시스템에 nested struct, array, fixed-dimension vector가 있다 ([fdb-record-layer README](https://github.com/FoundationDB/fdb-record-layer)).
- [사실] FDB Document Layer는 MongoDB wire protocol을 말하는 stateless microserver이고, 영속 데이터는 모두 FoundationDB KV store에 저장된다 ([fdb-document-layer README](https://github.com/FoundationDB/fdb-document-layer)).
- [사실] TiDB는 각 행을 `tablePrefix{TableID}_recordPrefixSep{RowID}` → 컬럼 값으로 저장한다. unique index는 `..._indexPrefixSep{IndexID}_indexedColumnsValue` → RowID로, non-unique index는 key 끝에 RowID를 붙이고 value를 null로 둔다. prefix를 공유하기 때문에 한 테이블의 행이 RowID 순으로 모여 있고, 전체 테이블을 range query로 효율적으로 스캔할 수 있다 ([TiDB Computing](https://docs.pingcap.com/tidb/stable/tidb-computing)).
- [사실] CockroachDB는 table ID, index ID, primary key 값, column family ID를 각각 인코딩해서 이어 붙여 KV key를 만든다. 한 행은 column family마다 KV pair 하나씩, 하나 이상을 만든다. secondary index는 행마다 KV pair 하나를 만든다. 인코딩이 order-preserving이고 prefix-free여서 SQL 계층과 KV 계층의 primary key 정렬이 같다 ([CockroachDB encoding tech note](https://github.com/cockroachdb/cockroach/blob/master/docs/tech-notes/encoding.md)).
- [사실] PostgreSQL은 테이블과 index를 고정 크기 page(보통 8 kB)의 배열로 저장한다. page 안에는 (offset, length) item identifier 배열이 있고, tuple header에 `t_xmin`(insert XID)과 `t_xmax`(delete XID)가 들어 있다. CTID는 page 번호와 item identifier 인덱스로 이뤄진다 ([PostgreSQL Page Layout](https://www.postgresql.org/docs/current/storage-page-layout.html)).
- [사실] BusTub은 CMU 15-445/645용 교육용 RDBMS다 ([BusTub README](https://github.com/cmu-db/bustub)). 과제는 C++ Primer, Buffer Pool Manager, B+Tree index, Query Execution, Concurrency Control 순서로 진행된다 ([15-445 Fall 2024 Assignments](https://15445.courses.cs.cmu.edu/fall2024/assignments.html)).

### 장단점
| | Ordered KV | Table/row-native |
|---|---|---|
| 다중 모델(relational/document/vector) 확장 | key encoding layer만 추가하면 된다. FDB, TiDB, CRDB가 이 방식으로 증명했다 [사실] | 모델마다 별도 storage format이 필요할 수 있다 [추론] |
| 엔진 핵심의 크기 | 스키마를 몰라도 되고 bytes만 다룬다 [추론] | tuple format, catalog, slotted page가 엔진 핵심에 들어간다 [사실: PG page layout] |
| 학습 초점(스토리지+동시성) | WAL, compaction, MVCC, 트랜잭션에 집중할 수 있다 [추론] | 쿼리 실행과 tuple 처리에 시간이 분산된다 [추론] |
| 약점 | 행 단위 접근에도 encoding/decoding 비용이 든다. key 설계(memcomparable encoding)를 직접 정확히 해야 한다 [추론] | 다른 모델을 얹기 어렵다 [추론] |

### 이 프로젝트에 대한 함의
- [추론] 장기 목표(relational/document/vector)와 2개월 범위(스토리지+동시성)를 함께 놓고 보면, 엔진 경계를 "정렬된 bytes KV + 트랜잭션"으로 좁히는 쪽이 범위 관리에 유리하다. 실제 시스템(FDB, TiKV/TiDB, CockroachDB/Pebble)이 같은 분리를 택했다.
- **[권고]** 엔진 API는 **ordered KV**(`get/put/delete/scan(range)` + transaction)로 둔다. relational과 document는 TiDB/CRDB 스타일 key encoding을 쓰는 얇은 layer crate로 나중에 얹는다. vector는 KV에 저장하더라도 ANN 인덱스가 별도 구조라서 기간 밖에 둔다 ([미검증]: vector 인덱스를 KV 위에 효율적으로 얹는 방법은 이번 조사 범위 밖).

---

## 2. 저장 구조: LSM-tree vs B+Tree + buffer pool

### 사실: 원전과 증폭(amplification)
- [사실] LSM-tree 원 논문(O'Neil et al., Acta Informatica 33, 1996)은 LSM-tree를 insert/delete 비율이 높은 파일에 저비용 indexing을 제공하는 디스크 자료구조로 설명한다. index 변경을 미루고 묶어서 memory component에서 disk component로 merge sort 비슷하게 흘려 보내며, B-tree보다 disk arm 이동을 크게 줄인다 ([O'Neil et al. 1996 preprint](https://www.cs.umb.edu/~poneil/lsmtree.pdf), [DOI](https://www.doi.org/10.1007/S002360050048)).
- [사실] RocksDB Tuning Guide는 세 가지 증폭을 정의한다. write amplification은 "the ratio of bytes written to storage versus bytes written to the database", read amplification은 "the number of disk reads per query", space amplification은 "the ratio of the size of database files on disk to data size"다. compaction이 이 셋 사이의 trade-off를 바꾸는 핵심 수단이다. leveled 예시(500 GB, multiplier 10)에서는 write amp ≈ 33, space amp ≈ 1.14다. universal compaction은 write amp를 낮추지만 space amp를 늘리며, 기본값 `max_size_amplification_percent=200`이면 100 byte DB가 최대 300 byte를 쓸 수 있다 ([RocksDB Tuning Guide](https://github.com/facebook/rocksdb/wiki/RocksDB-Tuning-Guide)).
- [사실] RUM Conjecture(Athanassoulis et al., EDBT 2016)는 access method 설계를 Read, Update, Memory(공간) overhead 세 가지로 보고, 둘을 최적화하면 나머지 하나가 나빠진다고 추측한다 ([RUM paper PDF](https://users.cs.utah.edu/~pandey/courses/cs6530/fall22/papers/indexdesign/rum.pdf), [OpenProceedings](http://openproceedings.org/2016/conf/edbt/paper-12.pdf)). (Harvard 페이지는 403이었고, PDF 사본과 저자 슬라이드로 확인했다.)

### 사실: 동시성
- [사실] CockroachDB의 Pebble은 LSM이다. 각 level의 SST 파일은 immutable이다 ([CockroachDB Storage Layer](https://docs.cockroachlabs.com/docs/stable/architecture/storage-layer)).
- [사실] B+Tree에서는 latch crabbing/coupling을 쓴다. parent latch를 잡고, child latch를 잡은 뒤, child가 "safe"(insert 시 split되지 않음, delete 시 merge되지 않음)면 parent latch를 놓는다. 개선판은 leaf까지 READ latch로 내려가고, leaf가 unsafe하면 처음부터 WRITE latch로 다시 시작한다. top-down 획득에서는 deadlock이 생기지 않지만 leaf scan은 양방향이라 deadlock 위험이 있다. 또 lock(트랜잭션 수준, 논리적, rollback 필요)과 latch(스레드 수준, 자료구조 보호, 짧게 보유)를 구분한다 ([CMU 15-445 Fall 2021 Lecture #08 notes](https://15445.courses.cs.cmu.edu/fall2021/notes/08-indexconcurrency.pdf)).

### 사실: 복구
- [사실] RocksDB는 WAL을 두고, 재시작하면 log에 기록된 트랜잭션을 다시 처리한다 ([RocksDB Overview](https://github.com/facebook/rocksdb/wiki/RocksDB-Overview)). WAL recovery mode로 꼬리 손상을 어떻게 다룰지 정한다. `kTolerateCorruptedTailRecords`, `kAbsoluteConsistency`, `kPointInTimeRecovery`(6.6부터 기본값), `kSkipAnyCorruptedRecords`가 있다 ([WAL Recovery Modes](https://github.com/facebook/rocksdb/wiki/WAL-Recovery-Modes)).
- [사실] ARIES(Mohan et al., ACM TODS 17(1), 1992)는 write-ahead logging 기반 복구 방법으로 fine-granularity(record) locking과 partial rollback을 지원한다. restart 때 "repeating history"로 빠진 update를 모두 redo한 뒤 loser 트랜잭션을 undo한다. page마다 LSN을 두고, rollback 중 수행한 update도 log한다 ([ARIES 초록, Brandeis](https://www.cs.brandeis.edu/~liuba/abstracts/mohan.html), [논문 PDF](https://dsf.berkeley.edu/cs262/Aries.pdf)).
- [사실] PostgreSQL은 torn page(8 kB 쓰기 중 일부 sector만 기록되는 경우)에 대비해 page를 수정하기 전에 full page image를 WAL에 쓴다(`full_page_writes`) ([PostgreSQL WAL Reliability](https://www.postgresql.org/docs/current/wal-reliability.html)).

### 사실: MVCC 결합 방식
- [사실] **RocksDB**: snapshot은 내부 sequence number다. key의 sequence number가 snapshot보다 크면 그 snapshot에서 보이지 않는다. flush와 compaction(`CompactionIterator`)은 살아 있는 snapshot이 볼 수 있는 버전을 보존한다 ([RocksDB Snapshot](https://github.com/facebook/rocksdb/wiki/Snapshot)).
- [사실] **TiKV (Percolator)**: `CF_DEFAULT (key,start_ts)→value`, `CF_LOCK key→lock_info`, `CF_WRITE (key,commit_ts)→write_info`로 나눠 저장한다. timestamp는 bit를 반전한 big-endian으로 key 뒤에 붙여서 최신 버전이 먼저 오게 한다 ([TiKV Percolator](https://tikv.org/deep-dive/distributed-transaction/percolator/)). TiKV는 저장 엔진으로 RocksDB를 쓴다 ([TiKV RocksDB](https://tikv.org/deep-dive/key-value-engine/rocksdb/)).
- [사실] **CockroachDB**: HLC timestamp로 버전을 구분하고 MVCC 데이터를 모두 Pebble에 저장한다. 오래된 버전은 GC 기간이 지나면 compaction으로 정리한다 ([CockroachDB Storage Layer](https://docs.cockroachlabs.com/docs/stable/architecture/storage-layer)).
- [사실] **InnoDB**: 행에 `DB_TRX_ID`와 `DB_ROLL_PTR`을 두고, 이전 버전은 rollback segment의 undo log로 재구성한다. clustered index는 in-place update하지만 secondary index는 delete-mark 후 insert한다. 실제 제거는 purge가 한다 ([InnoDB Multi-Versioning](https://dev.mysql.com/doc/refman/8.0/en/innodb-multi-versioning.html)).
- [사실] **PostgreSQL**: UPDATE와 DELETE가 old row version을 바로 지우지 않으므로 VACUUM이 필요하다. 32-bit XID wraparound를 막으려면 20억 트랜잭션마다 freeze가 필요하다 ([PostgreSQL Routine Vacuuming](https://www.postgresql.org/docs/current/routine-vacuuming.html)).

### 비교
| 항목 | LSM-tree | B+Tree + buffer pool |
|---|---|---|
| 증폭 | write 친화적이고 read/space amp는 compaction으로 조절한다 [사실: RocksDB Tuning Guide, O'Neil] | read 친화적이고 in-place update [추론, RUM 관점] |
| 동시성 | SST가 immutable이라 읽기 경로 latch가 적다. memtable만 concurrent 구조면 된다 [사실: Pebble immutable SST / 추론] | page latch와 latch crabbing이 필요하고 split/merge 경합을 처리해야 한다 [사실: CMU notes] |
| 복구 | WAL replay → memtable 재구성 + manifest. 데이터 파일은 덮어쓰지 않는다 [사실: RocksDB Overview / 추론] | ARIES(analysis/redo/undo, page LSN, CLR), torn page 대책 필요 [사실: ARIES, PG] |
| MVCC | key에 version(seq/ts)을 붙이는 방식이 자연스럽고, GC는 compaction이 맡는다 [사실: RocksDB, TiKV, CRDB] | undo log(InnoDB) 또는 tuple version + VACUUM(PG) [사실] |
| Rust 학습 자료 | mini-lsm: Rust로 LSM 엔진을 만드는 3주 × 7챕터 과정. SST, compaction, manifest, WAL, recovery, snapshot, MVCC, GC를 다룬다 [사실: [mini-lsm book](https://skyzh.github.io/mini-lsm/)] | BusTub은 C++이고 Rust 공식 대응판은 없다 [사실: 15-445 과제 목록 "C++ Primer"] |

### 이 프로젝트에 대한 함의
- [추론] 2개월 동안 "storage + crash recovery + MVCC 트랜잭션"까지 끝내야 한다. B+Tree 경로는 buffer pool, latch crabbing, ARIES식 undo/redo, torn page를 모두 직접 짜야 해서 범위가 크다. LSM 경로는 append-only 파일 + WAL replay라서 복구 모델이 단순하고, MVCC가 key versioning으로 바로 이어진다(RocksDB, TiKV, CRDB가 같은 구조).
- [추론] mini-lsm이 Rust로 정확히 같은 경로(week 3 MVCC 포함)를 다루므로 AI가 생성한 코드를 검증할 기준점이 생긴다. 반대로 "mini-lsm을 베꼈다"는 인상을 피하려면 차별점(fault injection 증거, durability 계약 문서화, 직접 설계한 트랜잭션 계층)이 필요하다.
- **[권고]** **LSM-tree**(leveled compaction 단순판). B+Tree, buffer pool, ARIES는 "왜 고르지 않았나"를 ADR에 RUM/amplification 근거로 남기는 것으로 대신한다. 면접에서 비교 설명을 할 수 있게 하기 위해서다.

---

## 3. 엔진 형태: embedded library + thin server vs server-first

### 사실
- [사실] RocksDB는 "a C++ library to store keys and values"이고, 한 프로세스 안에서 여러 embedded DB를 쓸 수 있다 ([RocksDB Overview](https://github.com/facebook/rocksdb/wiki/RocksDB-Overview)).
- [사실] SQLite는 별도 서버 프로세스 없이 애플리케이션과 같은 프로세스, 스레드, 주소 공간에서 돈다. 장점은 zero-configuration이다. 단점으로는 서버형이 클라이언트 버그(stray pointer)로부터 보호를 제공하고, 단일 영속 프로세스라서 더 세밀한 locking과 동시성 제어가 가능하다는 점을 스스로 든다 ([SQLite Serverless](https://www.sqlite.org/serverless.html)).
- [사실] LMDB는 "embedded, crash-proof" KV store다. MVCC에서 readers와 writers가 서로 막지 않고, write는 serialize되며, memory-mapped file로 zero-copy 조회를 한다. crash recovery 절차나 log가 필요 없다고 설명한다 ([Symas LMDB](https://www.symas.com/mdb)).
- [사실] sled는 "An embedded database"이고 API는 thread-safe `BTreeMap<[u8],[u8]>`과 비슷하다. beta이며 1.0 전에 on-disk format이 바뀐다고 경고한다 ([sled README](https://github.com/spacejam/sled)).
- [사실] TiKV는 저장 엔진으로 RocksDB를 쓴다. 서버(TiKV) 아래에 embedded 엔진(RocksDB)이 있는 구조다 ([TiKV RocksDB](https://tikv.org/deep-dive/key-value-engine/rocksdb/)). CockroachDB도 Pebble을 "an embedded key-value store"라고 부른다 ([CockroachDB Storage Layer](https://docs.cockroachlabs.com/docs/stable/architecture/storage-layer)).
- [사실] RocksDB 성능 측정의 주 도구인 `db_bench`는 엔진을 프로세스 안에서 직접 호출하는 벤치마크다(fillseq, fillrandom, readrandom, readwhilewriting, seekrandom 등) ([RocksDB Benchmarking tools](https://github.com/facebook/rocksdb/wiki/Benchmarking-tools)).

### 장단점 (추론)
| | Library-first (+ thin server) | Server-first |
|---|---|---|
| Fault injection | 파일 I/O를 trait(`Vfs`/`Fs`) 뒤로 숨기면 crash 지점, 부분 쓰기, fsync 실패를 테스트 안에서 주입할 수 있다 | 프로세스 kill 수준이 기본이고, 세밀한 주입은 별도 장치가 필요하다 |
| Deterministic testing | 단일 프로세스와 단일 스레드로 seed 재현이 가능하다 (FDB, TigerBeetle의 시뮬레이터도 전체 시스템을 단일 프로세스에서 돌린다 [사실, 6절]) | 네트워크와 스케줄링 때문에 비결정성이 커진다 |
| 벤치마크 | criterion, db_bench식 in-process 측정으로 엔진 자체 성능을 분리해 볼 수 있다 | 네트워크와 직렬화 비용이 섞인다 |
| 포트폴리오 가시성 | "서버로 띄워서 붙는" 데모가 약하다 → thin server로 보완 | 데모하기 쉽다 |
| 격리 | 클라이언트 버그가 엔진 메모리를 오염시킬 수 있다 (SQLite 자체 언급 [사실]). Rust safe code에서는 위험이 줄어든다 [추론] | 프로세스 격리 |

### 이 프로젝트에 대한 함의
- **[권고]** **library crate 우선**(`engine` crate) + 마지막 주에 **thin server**(예: RESP나 간단한 TCP line protocol). Cloud/Infra 직무 어필용으로는 서버 바이너리 + Docker 이미지 정도면 충분하다 [추론]. 파일 시스템 접근은 처음부터 trait 경계로 감싸서 5, 6절의 fault injection이 가능하게 한다.

---

## 4. Durability 계약: fsync per commit vs group commit vs configurable

### 실제 시스템의 노브와 보장 (사실)
| 시스템 / 설정 | 동작 | crash 시 손실 |
|---|---|---|
| RocksDB `WriteOptions.sync=true` | 반환 전에 WAL을 fsync한다 | 손실 없음 (커밋 확인 기준) |
| RocksDB `sync=false` (기본) | OS page cache까지만 보장한다 | WAL 쓰기는 "not crash safe" |
| RocksDB `manual_wal_flush` | WAL이 page cache에도 가지 않고 RocksDB 메모리에 남아 있다가 `FlushWAL()` 때 내려간다 | 프로세스 crash만으로도 손실 가능 |
| RocksDB group commit | 조건이 맞는 대기 중 write를 묶어 WAL에 한 번 쓰고 fsync도 한 번 한다(최대 1 MB). 배치를 키우려고 일부러 기다리지는 않는다 | sync 설정을 따른다 |

출처: [RocksDB WAL Performance](https://github.com/facebook/rocksdb/wiki/WAL-Performance)

| 시스템 / 설정 | 동작 | crash 시 손실 |
|---|---|---|
| LevelDB `sync=false` (기본) | 쓰기는 비동기다 | 프로세스 crash로는 손실 없음. **머신** crash면 마지막 몇 개 update가 사라질 수 있다. 배치(`WriteBatch`) + sync로 비용을 분산하라고 권한다 |

출처: [LevelDB doc](https://github.com/google/leveldb/blob/main/doc/index.md)

| 시스템 / 설정 | 동작 | crash 시 손실 |
|---|---|---|
| PostgreSQL `synchronous_commit=on/local` | commit 전에 WAL을 로컬 flush한다 | 없음 |
| PostgreSQL `synchronous_commit=off` | 기다리지 않는다 | 최근 커밋이 사라질 수 있다(최대 `wal_writer_delay`의 3배). DB 상태는 그 트랜잭션들이 깨끗하게 abort된 것과 같다 |
| PostgreSQL `commit_delay` / `commit_siblings` | 최소 `commit_siblings`(기본 5)개의 트랜잭션이 활성일 때 flush 전에 지연해서 group commit을 늘린다 | 손실 없음. latency만 늘어난다 |
| PostgreSQL `fsync=off` | — | 전원 장애나 시스템 crash 시 복구 불가능한 손상. "High quality hardware alone is not a sufficient justification" |

출처: [PostgreSQL WAL config](https://www.postgresql.org/docs/current/runtime-config-wal.html)

| 시스템 / 설정 | 동작 | crash 시 손실 |
|---|---|---|
| InnoDB `innodb_flush_log_at_trx_commit=1` (기본) | 커밋마다 write + flush. "required for full ACID compliance" | 없음 |
| `=0` | 초당 1회 write + flush | flush 안 된 트랜잭션 손실 가능 |
| `=2` | 커밋마다 write, 초당 1회 flush | flush 안 된 트랜잭션 손실 가능 |

출처: [InnoDB Startup Options](https://dev.mysql.com/doc/refman/8.0/en/innodb-parameters.html). ([미검증] "=2는 mysqld 프로세스 crash만으로는 손실이 없다"는 흔한 설명은 이번에 가져온 페이지 구간에서 직접 문구를 확인하지 못했다.)

### fsyncgate 교훈 (사실)
- [사실] 2018년 PostgreSQL 메일링 리스트에서 "fsync 성공 = 데이터가 디스크에 있음"이라는 가정이 일부 OS에서 깨진다는 것이 드러났다. Linux 4.13 이전에는 오류 후 buffer가 clean으로 표시돼서 재시도한 fsync가 거짓 성공할 수 있었다. 이후 버전에서도 오류는 한 번만 보고되므로 "retrying fsync() is not OK"다. macOS/Darwin, NetBSD, OpenBSD는 buffer를 invalidate해서 이후 fsync가 성공할 수 있다. PostgreSQL은 fsync 실패 시 PANIC하도록 바꾸고 9.4까지 backpatch했다 ([PostgreSQL wiki: Fsync Errors](https://wiki.postgresql.org/wiki/Fsync_Errors)).
- [사실] `data_sync_retry=off`(기본)이면 flush 실패 시 PANIC한다. write-back 실패 뒤 page cache 상태를 알 수 없으므로, 유일한 안전책은 WAL로부터 복구하는 것이다 ([PostgreSQL Error Handling](https://www.postgresql.org/docs/current/runtime-config-error-handling.html)).
- [사실] fsync는 파일의 directory entry까지 보장하지 않는다. 디렉터리 fd도 fsync해야 한다. `fdatasync`는 이후 읽기에 필요하지 않은 metadata flush를 생략한다(파일 크기 변경은 flush한다) ([Linux fsync(2)](https://man7.org/linux/man-pages/man2/fsync.2.html)).
- [사실] ALICE(OSDI'14)는 persistence property가 6개의 Linux 파일시스템 사이에서도 크게 다르고, 널리 쓰이는 애플리케이션 11개에서 crash 취약점 60개를 찾았다고 보고했다 ([Pillai et al., OSDI'14](https://www.usenix.org/conference/osdi14/technical-sessions/presentation/pillai)).

### 이 프로젝트에 대한 함의
- [추론] 위 시스템들은 모두 "기본은 안전, 성능을 위해 명시적으로 약화"하거나(PG, InnoDB의 기본 1), "기본은 빠름, sync는 옵트인"(RocksDB, LevelDB)으로 **계약을 문서화하고 노브로 노출**한다. 포트폴리오에서 평가받는 것은 노브의 개수가 아니라 "각 모드에서 무엇이 사라지는지 정확히 말하고 테스트로 보여 주는 것"이다.
- **[권고]**
  1. `SyncMode { PerCommit, GroupCommit, None }` 같은 **configurable** 계약으로 두되 **기본값은 PerCommit(안전)**으로 한다. GroupCommit은 RocksDB식 "리더가 대기 중 write를 묶어 한 번 fsync"로 구현한다(일부러 지연시키지 않음).
  2. fsync 실패는 **재시도하지 않고 엔진을 poison(panic/read-only)**한다. 재시작하면 WAL로 복구한다(fsyncgate, `data_sync_retry` 교훈).
  3. 새 파일(WAL segment, SST, MANIFEST)을 만들거나 rename한 뒤에는 **디렉터리 fsync**를 한다.
  4. WAL 레코드에 checksum + length를 넣고, 꼬리 손상은 RocksDB `kTolerateCorruptedTailRecords`/`kPointInTimeRecovery`와 같은 정책으로 문서화한다.
  5. README에 모드별 "손실 가능 범위" 표를 넣는다(위 PG/InnoDB 표 형식).

---

## 5. 개발/테스트 플랫폼: macOS vs Linux, fault injection 도구

### 사실
- [사실] macOS `fsync(2)`: host에서 drive로 데이터를 넘길 뿐, drive가 실제로 언제 기록할지와 그 순서는 보장하지 않는다. 전원 손실이나 OS crash 때 일부 또는 전부가 없을 수 있고, 순서가 뒤바뀔 수 있다. 엄격한 쓰기 순서가 필요한 DB는 `F_FULLFSYNC`를 쓰라고 한다 ([Apple fsync(2)](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/fsync.2.html)).
- [사실] `F_FULLFSYNC`는 fsync를 한 뒤 drive에 모든 buffer를 영구 저장장치로 flush하라고 요청한다. 문서상 구현은 HFS, MS-DOS(FAT), UDF이고, 일부 FireWire 드라이브는 이 요청을 무시한다고 적혀 있다 ([Apple fcntl(2)](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/fcntl.2.html)). [미검증] 이 archive man page는 오래된 문서라서 APFS 지원 여부를 명시하지 않는다. 현재 macOS의 APFS 동작은 1차 출처로 확인하지 못했다.
- [사실] SQLite `PRAGMA fullfsync`(기본 off)는 F_FULLFSYNC 사용 여부를 정하며, "Only Mac OS X supports F_FULLFSYNC"라고 한다 ([SQLite PRAGMA](https://www.sqlite.org/pragma.html)).
- [사실] PostgreSQL은 macOS에서 write caching을 막으려면 `wal_sync_method=fsync_writethrough`를 쓰라고 한다. 소비자용 드라이브와 많은 SSD에 volatile write-back cache가 있고, 많은 SSD가 기본으로 cache flush 명령을 따르지 않는다고 경고한다 ([PostgreSQL WAL Reliability](https://www.postgresql.org/docs/current/wal-reliability.html)).
- [사실] Linux `fsync`는 디스크 cache가 있으면 write-through/flush한다. 오래된 커널이나 덜 쓰이는 파일시스템은 예외일 수 있다 ([Linux fsync(2)](https://man7.org/linux/man-pages/man2/fsync.2.html)).
- Fault injection 도구
  - [사실] **dm-flakey**: device-mapper target이다. up/down 주기로 down 구간에 I/O 오류(기본), `drop_writes`(쓰기 조용히 버림), `error_writes`, `corrupt_bio_byte`를 일으킨다 ([kernel docs: dm-flakey](https://www.kernel.org/doc/html/latest/admin-guide/device-mapper/dm-flakey.html)).
  - [사실] **dm-log-writes**: 모든 WRITE를 별도 장치에 log한다. 일반 WRITE는 다음 PREFLUSH 때 log되고 FUA는 즉시 log된다. 특정 시점까지 replay해서 전원 장애 시점의 상태를 검사한다 ([kernel docs: dm-log-writes](https://www.kernel.org/doc/html/latest/admin-guide/device-mapper/log-writes.html)).
  - [사실] **CrashMonkey**: 파일시스템에 구애받지 않는 record-replay-test 프레임워크다. README 배지는 "Experimental"이고, 테스트된 커널은 3.12~5.6, 권장 환경은 Ubuntu 16.04 VM이다 ([CrashMonkey README](https://github.com/utsaslab/crashmonkey)).
  - [사실] **LazyFS**: 자체 page cache를 둔 FUSE 파일시스템으로, 명시적으로 요청할 때만 flush한다. `clear-cache`(unsynced 데이터 버림), `torn-seq`, `torn-op` fault를 제공한다. Debian 11과 Ubuntu 20.04, ext4, FUSE 3에서 테스트됐다 ([LazyFS README](https://github.com/dsrhaslab/lazyfs)).
  - [사실] **turmoil**(Rust): 단일 스레드 deterministic 시뮬레이션이다. `unstable-fs` feature로 `std::fs`를 흉내 낸 시뮬레이션 FS를 제공한다. write는 `sync_all()` 전까지 pending 상태이고, `Sim::crash`는 pending write를 버린다 ([turmoil docs](https://docs.rs/turmoil/latest/turmoil/)).
- [사실] Docker Desktop(Mac)은 Linux VM 위에서 컨테이너를 돌리고, 파일 공유는 VirtioFS(기본)나 gRPC FUSE를 쓴다. 문서는 fsync나 durability를 다루지 않는다 ([Docker Desktop settings](https://docs.docker.com/desktop/settings-and-maintenance/settings/)).

### 추론 / 미검증
- [미검증] Docker Desktop VM 안에서 호출한 fsync가 호스트 Mac SSD까지 F_FULLFSYNC 수준으로 내려가는지는 1차 출처로 확인하지 못했다. **VM이나 컨테이너 안에서 측정한 "전원 손실 내구성"은 증거로 쓰지 않는 것이 안전하다.**
- [추론] dm-flakey와 dm-log-writes는 Linux device-mapper라서 root와 블록 장치(loop device)가 필요하다. Mac에서는 Linux VM(예: Lima/UTM/OrbStack, 또는 클라우드 VM)이나 privileged 컨테이너가 필요할 것이다. [미검증] 특정 VM 도구의 지원 여부는 확인하지 않았다.
- [추론] 진짜 전원 손실 테스트는 하드웨어 차원이라 2개월 범위 밖이다. 현실적인 목표는 "**애플리케이션 수준 crash consistency**": fsync하지 않은 데이터가 사라지거나 쓰기가 찢어져도 엔진이 일관된 상태로 복구되는지를 보이는 것이다. 이것은 FS trait 기반 in-process 시뮬레이션과 LazyFS로 충분히 보일 수 있다.

### 권고
- **[권고]**
  1. 일상 개발은 **macOS**에서 한다. 엔진의 sync 호출은 플랫폼별로 분기한다: macOS는 `F_FULLFSYNC`(Rust `std`의 `File::sync_all`이 macOS에서 무엇을 호출하는지는 [미검증], 구현할 때 표준 라이브러리 소스로 확인할 것), Linux는 `fdatasync`(WAL) / `fsync`(새 파일 + 디렉터리).
  2. **1차 crash 테스트는 in-process**로 한다: `trait Fs`의 시뮬레이션 구현(LazyFS/turmoil처럼 unsynced write를 버퍼에 두고 crash 시 버리거나 일부만 남기는 방식)으로 seed 기반 재현 테스트를 한다.
  3. **2차는 Linux VM에서 LazyFS**로 실제 바이너리를 대상으로 `clear-cache`/`torn-op` 시나리오를 한두 개 돌린다. dm-flakey와 dm-log-writes는 시간이 남으면 시도하고, CrashMonkey는 유지 상태 때문에 비권장이다.
  4. CI는 GitHub Actions Linux runner에서 1번 테스트를 돌린다. Docker 안의 durability 결과는 "기능 테스트"로만 표기한다.

---

## 6. 검증·증거 산출물 (가벼운 워크플로)

### 사실
- [사실] **FoundationDB**: 전체 클러스터를 단일 스레드 프로세스 안에서 deterministic하게 시뮬레이션한다. Flow가 실제 실행과 시뮬레이션 실행을 모두 지원하고, 머신 장애 등을 짧은 간격으로 주입한다. "It seems unlikely that we would have been able to build FoundationDB without this technology" ([FDB Testing](https://apple.github.io/foundationdb/testing.html)).
- [사실] **TigerBeetle VOPR**: clock, network, disk를 stub으로 바꿔 비결정성을 없앤다. seed + git commit으로 같은 시뮬레이션과 버그를 재현한다. 패킷 drop/reorder, partition, 디스크 read/write 손상을 주입한다 ([TigerBeetle VOPR](https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/internals/vopr.md)). 디스크가 "will fail"한다고 가정하고, 모든 데이터를 checksum하고 hash-chain으로 묶는다 ([TigerBeetle Safety](https://docs.tigerbeetle.com/concepts/safety/)).
- [사실] **Jepsen**: 실제 바이너리를 실제 클러스터에서 돌리면서 네트워크와 시계 장애를 주입하고, 랜덤 동시 연산의 history를 정합성 모델과 대조한다. 대상은 주로 분산 DB, coordination 서비스, 큐다 ([Jepsen analyses](https://jepsen.io/analyses)).
- [사실] **proptest**: "Hypothesis-like property-based testing and shrinking" ([proptest docs](https://docs.rs/proptest/latest/proptest/)).
- [사실] **loom**: C11 메모리 모델이 허용하는 interleaving을 탐색하는 permutation 테스트 도구다. loom 타입으로 바꾼 연산만 볼 수 있고(intrusive), 큰 모델은 느리다. preemption bound 2~3이면 대부분의 버그를 잡는다고 한다. `Relaxed`의 모든 재배치를 흉내 내지는 못한다 ([loom docs](https://docs.rs/loom/latest/loom/)).
- [사실] **madsim**: deterministic simulation을 하는 tokio 대체 async runtime이며 RisingWave가 사용한다 ([madsim README](https://github.com/madsim-rs/madsim)). **turmoil**은 5절 참조.
- [사실] **Criterion.rs**: 통계 기반 micro-benchmark로 성능 회귀를 자동 감지한다 ([Criterion.rs book](https://bheisler.github.io/criterion.rs/book/index.html)).
- [사실] **db_bench**: RocksDB의 주 벤치마크 도구로 LevelDB에서 물려받았다 ([RocksDB Benchmarking tools](https://github.com/facebook/rocksdb/wiki/Benchmarking-tools)).
- [사실] **YCSB core workloads**: A(50/50 read/update), B(95/5), C(read only), D(read latest + insert), E(short range scan + insert), F(read-modify-write) ([YCSB Core Workloads](https://github.com/brianfrankcooper/YCSB/wiki/Core-Workloads)).

### 추론
- [추론] Jepsen은 분산 시스템과 다중 노드를 대상으로 하므로 단일 노드 엔진에는 과하다. 다만 "동시 연산 history를 기록하고 isolation 모델로 검사한다"는 아이디어는 트랜잭션 검증에 그대로 쓸 수 있다.
- [추론] FDB와 TigerBeetle 수준의 전면 DST는 2개월 안에 하기에는 무겁다. 하지만 **엔진의 I/O를 trait로 추상화하고 seed로 crash 지점을 고르는** 축소판은 현실적이고, 포트폴리오에서 가장 차별화되는 산출물이 될 가능성이 크다.

### 권고: 2개월 Rust 프로젝트용 검증 툴킷
| 계층 | 도구 | 증거 산출물 |
|---|---|---|
| 1. 모델 기반 정합성 | `proptest`: 랜덤 연산 시퀀스를 엔진과 `BTreeMap` 참조 모델에 동시에 적용해 비교 (get/scan/delete/재시작) | shrink된 반례가 회귀 테스트로 남음 |
| 2. Crash consistency (in-process DST 축소판) | `trait Fs` 시뮬레이터 + seed: 임의 시점 crash, unsynced write 폐기/부분 적용 → 재오픈 → "확인된 커밋은 모두 존재하고, 미확인 커밋은 원자적으로 존재하거나 없음" 검사 | `cargo test` 수천 seed 결과, 실패 seed 재현 명령 |
| 3. 동시성 프리미티브 | `loom`: memtable/WAL group commit 큐 등 lock-free나 condvar를 쓰는 소수의 핵심 구조에만 적용 | loom 테스트 통과 로그 |
| 4. 트랜잭션 isolation | 다중 스레드 랜덤 트랜잭션 history 기록 → snapshot isolation 위반(lost update, write skew 허용 여부 명시) 검사기 | 이상 현상 표와 테스트 |
| 5. 실제 FS crash (선택) | Linux VM + LazyFS `clear-cache`/`torn-op` 시나리오 1~2개 | 스크립트와 결과 로그 |
| 6. 성능 | `criterion`(micro: memtable insert, SST lookup), db_bench 스타일 자체 bench(fillseq/fillrandom/readrandom/readwhilewriting), YCSB A/B/C/E 비율을 흉내 낸 워크로드. sync 모드별 throughput/latency 비교 | 그래프와 README 표 (SyncMode × 워크로드) |

- **[권고]** 결정마다 ADR 1개, 기능마다 "계약(무엇을 보장하나) → 테스트(어떻게 보였나) → 수치(얼마나 빠른가)" 3단 README 섹션을 둔다. 무거운 프로세스(Jepsen, 전면 DST, 하드웨어 전원 차단)는 명시적으로 범위 밖으로 둔다.

---

## 요약 권고 (의견)
1. **데이터 모델**: ordered KV + 트랜잭션을 엔진 경계로 두고, relational/document는 key-encoding layer로 얹는다.
2. **저장 구조**: LSM-tree(leveled 단순판), MVCC는 key versioning. B+Tree/ARIES는 비교 ADR로 정리한다.
3. **엔진 형태**: library crate 우선, FS trait 경계, 마지막에 thin server.
4. **Durability**: configurable(PerCommit 기본, GroupCommit, None). fsync 실패 시 poison, 디렉터리 fsync, WAL checksum.
5. **플랫폼**: macOS 개발(F_FULLFSYNC 분기), in-process crash 시뮬레이션이 주력, Linux VM + LazyFS는 보조. Docker 결과는 durability 증거로 쓰지 않는다.
6. **검증**: proptest 모델 테스트, seed 기반 crash 시뮬레이션, loom(국소), isolation history 검사, criterion + db_bench/YCSB식 bench.

## 미검증 항목 모음
- InnoDB `innodb_flush_log_at_trx_commit=2`에서 "mysqld crash만으로는 손실 없음"이라는 문구(이번 조회 구간에서 미확인).
- 현재 macOS(APFS)에서 F_FULLFSYNC 지원과 동작(참조한 Apple man page는 archive 문서).
- Rust `std::fs::File::sync_all`이 macOS에서 `F_FULLFSYNC`를 호출하는지 여부.
- Docker Desktop/VM 안의 fsync가 호스트 저장장치 내구성으로 이어지는지 여부.
- 특정 Mac용 Linux VM 도구에서 dm-flakey/dm-log-writes/LazyFS를 쓸 수 있는지 여부.
- vector 인덱스를 ordered KV 위에 효율적으로 얹는 방법.
- "Files are hard"(Dan Luu 블로그)는 2차 자료라서 인용하지 않았고, 원 연구인 ALICE(OSDI'14)만 인용했다.

---

## 출처

**논문**
- O'Neil, Cheng, Gawlick, O'Neil. *The Log-Structured Merge-Tree (LSM-Tree)*. Acta Informatica 33, 1996. https://www.cs.umb.edu/~poneil/lsmtree.pdf / https://www.doi.org/10.1007/S002360050048
- Mohan et al. *ARIES: A Transaction Recovery Method Supporting Fine-Granularity Locking and Partial Rollbacks Using Write-Ahead Logging*. ACM TODS 17(1), 1992. https://dsf.berkeley.edu/cs262/Aries.pdf , 초록: https://www.cs.brandeis.edu/~liuba/abstracts/mohan.html
- Athanassoulis et al. *Designing Access Methods: The RUM Conjecture*. EDBT 2016. http://openproceedings.org/2016/conf/edbt/paper-12.pdf , https://users.cs.utah.edu/~pandey/courses/cs6530/fall22/papers/indexdesign/rum.pdf
- Pillai et al. *All File Systems Are Not Created Equal* (ALICE). OSDI 2014. https://www.usenix.org/conference/osdi14/technical-sessions/presentation/pillai

**RocksDB / LevelDB**
- RocksDB Overview: https://github.com/facebook/rocksdb/wiki/RocksDB-Overview
- RocksDB Tuning Guide: https://github.com/facebook/rocksdb/wiki/RocksDB-Tuning-Guide
- RocksDB WAL Performance: https://github.com/facebook/rocksdb/wiki/WAL-Performance
- RocksDB WAL Recovery Modes: https://github.com/facebook/rocksdb/wiki/WAL-Recovery-Modes
- RocksDB Snapshot: https://github.com/facebook/rocksdb/wiki/Snapshot
- RocksDB Benchmarking tools: https://github.com/facebook/rocksdb/wiki/Benchmarking-tools
- LevelDB doc: https://github.com/google/leveldb/blob/main/doc/index.md

**TiKV / TiDB / CockroachDB / FoundationDB**
- TiDB Computing (key encoding): https://docs.pingcap.com/tidb/stable/tidb-computing
- TiKV Percolator: https://tikv.org/deep-dive/distributed-transaction/percolator/
- TiKV RocksDB: https://tikv.org/deep-dive/key-value-engine/rocksdb/
- CockroachDB encoding tech note: https://github.com/cockroachdb/cockroach/blob/master/docs/tech-notes/encoding.md
- CockroachDB Storage Layer: https://docs.cockroachlabs.com/docs/stable/architecture/storage-layer
- FoundationDB Features: https://apple.github.io/foundationdb/features.html
- FoundationDB Layer Concept: https://apple.github.io/foundationdb/layer-concept.html
- FoundationDB Testing: https://apple.github.io/foundationdb/testing.html
- FDB Record Layer: https://github.com/FoundationDB/fdb-record-layer
- FDB Document Layer: https://github.com/FoundationDB/fdb-document-layer

**PostgreSQL / MySQL InnoDB / SQLite / LMDB / sled**
- PostgreSQL WAL Configuration: https://www.postgresql.org/docs/current/runtime-config-wal.html
- PostgreSQL Error Handling (data_sync_retry): https://www.postgresql.org/docs/current/runtime-config-error-handling.html
- PostgreSQL WAL Reliability: https://www.postgresql.org/docs/current/wal-reliability.html
- PostgreSQL Page Layout: https://www.postgresql.org/docs/current/storage-page-layout.html
- PostgreSQL Routine Vacuuming: https://www.postgresql.org/docs/current/routine-vacuuming.html
- PostgreSQL wiki, Fsync Errors: https://wiki.postgresql.org/wiki/Fsync_Errors
- MySQL InnoDB Startup Options and System Variables: https://dev.mysql.com/doc/refman/8.0/en/innodb-parameters.html
- MySQL InnoDB Multi-Versioning: https://dev.mysql.com/doc/refman/8.0/en/innodb-multi-versioning.html
- SQLite Serverless: https://www.sqlite.org/serverless.html
- SQLite PRAGMA: https://www.sqlite.org/pragma.html
- Symas LMDB: https://www.symas.com/mdb
- sled README: https://github.com/spacejam/sled

**교육 자료**
- mini-lsm book: https://skyzh.github.io/mini-lsm/
- BusTub: https://github.com/cmu-db/bustub
- CMU 15-445 Fall 2024 Assignments: https://15445.courses.cs.cmu.edu/fall2024/assignments.html
- CMU 15-445 Fall 2021 Lecture #08 Index Concurrency Control notes: https://15445.courses.cs.cmu.edu/fall2021/notes/08-indexconcurrency.pdf

**OS / 파일시스템 / fault injection**
- Linux fsync(2): https://man7.org/linux/man-pages/man2/fsync.2.html
- Apple fsync(2): https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/fsync.2.html
- Apple fcntl(2): https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/fcntl.2.html
- Linux kernel dm-flakey: https://www.kernel.org/doc/html/latest/admin-guide/device-mapper/dm-flakey.html
- Linux kernel dm-log-writes: https://www.kernel.org/doc/html/latest/admin-guide/device-mapper/log-writes.html
- CrashMonkey: https://github.com/utsaslab/crashmonkey
- LazyFS: https://github.com/dsrhaslab/lazyfs
- Docker Desktop settings: https://docs.docker.com/desktop/settings-and-maintenance/settings/

**테스트 / 벤치마크**
- TigerBeetle VOPR: https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/internals/vopr.md
- TigerBeetle Safety: https://docs.tigerbeetle.com/concepts/safety/
- Jepsen analyses: https://jepsen.io/analyses
- proptest: https://docs.rs/proptest/latest/proptest/
- loom: https://docs.rs/loom/latest/loom/
- turmoil: https://docs.rs/turmoil/latest/turmoil/
- madsim: https://github.com/madsim-rs/madsim
- Criterion.rs: https://bheisler.github.io/criterion.rs/book/index.html
- YCSB Core Workloads: https://github.com/brianfrankcooper/YCSB/wiki/Core-Workloads

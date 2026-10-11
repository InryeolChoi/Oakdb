# 0002. AI 시대에 SQLite가 못 하는 것(또는 약한 것) 조사

- 작성일: 2026-10-12
- 목적: Oakdb(Rust, B+Tree key-value 엔진 + 간단한 relational layer, 약 2개월, 코드는 대부분 AI가 작성)가 **SQLite와 차별화할 지점 2개**를 고르기 위한 근거를 1차 출처로 정리한다.
- 범위 전제: 핵심은 **스토리지 엔진 + crash recovery**, **동시성 + 트랜잭션**이다. SQL은 최소로 하고, 네트워크 서버는 최소한으로 만들어 벤치마크한다. 분산은 뒤로 미룬다.
- 이미 나온 후보: (A) 동시 writer, (B) 네트워크 client/server 접근, (C) 복제(replication)
- 표기 규칙 (0001과 같음)
  - **[사실]**: 링크한 1차 출처(공식 문서, 소스, 논문, 벤더 자체 블로그)에서 직접 확인한 내용
  - **[추론]**: 사실을 바탕으로 한 해석이나 의견
  - **[권고]**: 의견. 최종 결정은 ADR로 남긴다
  - **[미검증]**: 1차 출처로 직접 확인하지 못한 내용

---

## 요약 표

| 포인트 | SQLite 현재 | AI 시대에 왜 중요 | Oakdb 적합도 | 난이도 |
|---|---|---|---|---|
| 1. 동시 writer | 한 번에 writer 하나. `BEGIN CONCURRENT`는 별도 branch에만 있고 충돌 판정이 page 단위 | 여러 agent·백그라운드 작업이 같은 DB에 동시에 쓴다 [추론] | **상**: 동시성+트랜잭션 핵심 목표와 정확히 겹친다 | 중상 |
| 2. 브랜치·스냅샷·되돌리기 | 파일 복사(backup API, `VACUUM INTO`)와 WAL 안의 짧은 과거 스냅샷 읽기만 있다. 싸게 "갈라지는" 브랜치는 없다 | agent가 복사본에서 시도하고 실패하면 버린다. Neon은 DB 80% 이상을 agent가 만든다고 밝힘 [사실: 벤더 수치] | **중상**: B+Tree copy-on-write와 recovery 설계에 바로 이어진다 | 상 |
| 3. 변경 구독(CDC) | 같은 connection 안의 `update_hook`, 컴파일 옵션인 session extension만 있다. 다른 프로세스에 알려 주는 기능은 없다 | agent가 데이터 변화에 반응해서 움직인다 [추론] | **중**: WAL을 재사용하면 되지만 차별화 폭은 작다 | 중 |
| 4. 네트워크 접근 (기존 B) | 서버가 없다. 공식 문서가 "네트워크면 client/server를 쓰라"고 한다 | 서버리스·원격 agent가 HTTP로 붙는다 [추론] | **중**: 어차피 최소 서버를 만들 예정이라 "차별점"으로 쓰기엔 약하다 | 하~중 |
| 5. 복제 (기존 C) | 코어에 없다 (session extension으로 직접 만들어야 함) | 여러 위치의 agent에 가까운 사본 [추론] | **하**: 분산은 범위 밖 | 상 |
| 6. 사용자·권한 | DB 사용자 개념이 없다. user-auth 확장은 3.48.0에서 흔적까지 제거됐다. connection 단위 authorizer 콜백만 있다 | agent마다 권한을 달리 주고 싶다 [추론] | **하~중**: 서버가 있어야 의미가 있고 엔진 핵심과 거리가 멀다 | 중 |
| 7. 변경 이력·감사 로그 | system-versioned(temporal) table이 없다 | "어느 agent가 언제 무엇을 바꿨나" [추론] | **중**: MVCC 버전을 남기면 공짜에 가깝지만 GC와 충돌 | 중 |
| 8. 스키마 변경 | `ALTER TABLE`이 제한적. 나머지는 12단계 수동 절차 | AI가 스키마를 자주 바꾼다 [추론] | **하**: SQL layer 기능이라 엔진 학습 목표와 멀다 | 중 |
| 9. 관측성(쿼리 통계) | `sqlite3_stmt_status` 등 statement 단위 카운터가 있다 | 큰 gap 아님 | **하** | 하 |
| 10. AI가 만든 쿼리의 안전장치 | **이미 꽤 있다**: authorizer, `sqlite3_limit`, progress handler, `query_only`, heap limit | gap이 아니다 (거짓 gap 주의) | 해당 없음 | — |
| 11. 벡터 검색 | 코어에 없다. `sqlite-vec` 확장(pre-v1) | RAG·임베딩 | **범위 밖**(사용자가 제외) | — |

---

## 1. 동시 writer (기존 후보 A)

**비유**: 은행 창구가 하나뿐이라, 손님(writer)이 몇 명이 오든 한 줄로 서야 한다. 업무가 빨리 끝나면 괜찮지만, 손님이 수십 명으로 늘면 줄이 길어진다.

- **writer**: DB에 데이터를 쓰는 쪽. **트랜잭션**: "전부 되거나 전부 안 되거나"로 묶은 작업 단위.

### SQLite 현재
- [사실] "SQLite supports an unlimited number of simultaneous readers, but it will only allow one writer at any instant in time." 대부분 write는 몇 밀리초라서 줄을 서도 괜찮다고 덧붙인다 ([When To Use](https://www.sqlite.org/whentouse.html)).
- [사실] 같은 문서: 많은 스레드·프로세스가 같은 순간에 써야 하고 줄을 설 수 없다면 그 기능을 지원하는 엔진을 고르라고 하며, 그것은 "always means a client/server database engine"이라고 한다. client/server DB는 "can usually handle far more write concurrency than SQLite ever will" ([When To Use](https://www.sqlite.org/whentouse.html)).
- [사실] WAL 모드에서도 "since there is only one WAL file, there can only be one writer at a time" ([WAL](https://www.sqlite.org/wal.html)).
- [사실] `BEGIN CONCURRENT`는 WAL/wal2 모드에서 여러 writer가 동시에 트랜잭션을 진행하게 하지만 "the system still serializes COMMIT commands". 충돌은 "optimistic page-level-locking"으로 판정한다. 커밋할 때 읽은 page가 그 사이 바뀌었으면 `SQLITE_BUSY_SNAPSHOT`을 돌려주고 할 수 있는 일은 ROLLBACK뿐이다 ([begin_concurrent.md](https://www.sqlite.org/src/doc/begin-concurrent/doc/begin_concurrent.md)).
- [사실] 이 기능은 trunk가 아닌 별도 `begin-concurrent` branch에 있고, trunk 변경을 주기적으로 merge 받는다(최근 check-in 2026-10-09) ([Fossil timeline](https://www.sqlite.org/src/timeline?r=begin-concurrent&n=5)).
- **optimistic(낙관적) 동시성 제어**: 일단 각자 작업하게 두고, 커밋할 때 겹쳤는지 검사해서 겹치면 한쪽을 취소하는 방식.

### AI agent와의 관계
- [추론] 코딩 agent 여러 개, 백그라운드 작업, 도구 호출 로그 기록이 같은 DB 파일에 동시에 쓰는 구조가 흔해졌다. 하나하나는 짧은 write라도 동시에 몰리면 `SQLITE_BUSY` 처리 코드가 늘어난다.
- [사실] Turso(SQLite를 Rust로 다시 쓰는 회사)는 자체 블로그에서 "the old single-writer model that SQLite pioneered has started to show its age in today's concurrent world"라고 썼다. SQLite `BEGIN CONCURRENT`는 page 단위라서 같은 page의 서로 다른 행을 고쳐도 충돌로 처리돼 "even though no logical conflict has occurred" 한쪽이 abort된다고 지적하고, 자기들은 **행(row) 단위** 충돌을 커밋 시점에 검사하는 MVCC를 쓴다고 설명한다 ([Turso blog, 2025-10-06](https://turso.tech/blog/beyond-the-single-writer-limitation-with-tursos-concurrent-writes)). 이 글은 AI agent를 직접 언급하지는 않는다.
- [사실] Turso 문서: 기본 설정은 한 connection만 쓰기 가능하고, `PRAGMA journal_mode = 'mvcc'` + `BEGIN CONCURRENT`로 여러 connection이 동시에 쓴다. 같은 데이터를 고치면 한쪽이 conflict 오류를 받고 재시도해야 한다 ([Turso Concurrent Writes](https://docs.turso.tech/tursodb/concurrent-writes)).

### 다른 시스템
- [사실] PostgreSQL MVCC: "reading never blocks writing and writing never blocks reading". 필요하면 table/row-level lock도 쓸 수 있다 ([PG MVCC intro](https://www.postgresql.org/docs/current/mvcc-intro.html)).
- **MVCC(다중 버전 동시성 제어)**: 데이터를 덮어쓰지 않고 버전을 여러 개 남겨서, 읽는 사람은 자기 시점의 버전을 보고 쓰는 사람은 새 버전을 만드는 방식.

### Oakdb 적합도: **상**
- [추론] 프로젝트 핵심 목표(동시성 + 트랜잭션)와 정확히 겹친다. "SQLite는 page 단위, Oakdb는 key 단위 충돌 판정"은 벤치마크 한 장(같은 page에 몰린 서로 다른 key를 여러 writer가 갱신할 때 abort율·처리량)으로 보여 줄 수 있다.

### 난이도: 중상
- [추론] 커밋만 직렬화하고 실행은 병렬로 두는 OCC(SQLite·Turso와 같은 골격)라면 2개월 안에 가능하다. 동시에 B+Tree에 쓰는 부분(latch)과 crash recovery를 함께 맞추는 데 시간이 든다.

---

## 2. 브랜치·스냅샷·되돌리기

**비유**: 문서를 고치기 전에 "사본 만들기"를 누르는 것. 다만 100GB 문서를 통째로 복사하면 느리니, **바뀐 부분만** 따로 적어 두는 사본(copy-on-write)을 만든다.

- **스냅샷**: 특정 시점의 DB 모습. **브랜치**: 스냅샷에서 갈라져 나와 독립적으로 고칠 수 있는 사본. **copy-on-write(COW)**: 원본을 공유하다가 고치는 순간에만 그 부분을 복사하는 방식.

### SQLite 현재
- [사실] online backup API는 DB를 다른 파일로 통째로 복사해 "bit-wise identical copy"(스냅샷)를 만든다. 다른 connection이 중간에 쓰면 보통 처음부터 다시 시작하며, 자주 쓰이면 "may never run to completion" ([Backup API](https://www.sqlite.org/backup.html)).
- [사실] `VACUUM INTO`는 "a consistent snapshot of the original database"를 새 파일로 만든다 ([VACUUM](https://www.sqlite.org/lang_vacuum.html)).
- [사실] `sqlite3_snapshot_open()`은 읽기 트랜잭션을 과거 스냅샷에 맞출 수 있지만, `SQLITE_ENABLE_SNAPSHOT` 컴파일 옵션과 WAL 모드가 필요하고 checkpoint로 덮어써지면 `SQLITE_ERROR_SNAPSHOT`으로 실패한다 ([snapshot_open](https://www.sqlite.org/c3ref/snapshot_open.html)). 즉 **읽기 전용, 짧은 과거**만 된다.
- [추론] 둘 다 "전체 복사"라서 DB 크기에 비례하는 시간·공간이 든다. 싸게 갈라지고 쓸 수 있는 브랜치는 SQLite에 없다.

### AI agent와의 관계
- [사실] Neon(서버리스 Postgres) 공식 블로그: "within a few months, over 80% of databases were being created by AI agents rather than humans" ([Neon blog, 2025-05-14](https://neon.com/blog/neon-and-databricks)). 벤더 자체 telemetry 수치다.
- [사실] Neon의 agent용 페이지: agent가 스키마를 마이그레이션하거나 "revert mistakes without complex restores" 하도록 branching을 쓰고, Snapshots API로 "a moment-in-time version of the database (schema and data)"를 잡아 나중에 되돌리거나 비교한다 ([Neon for AI Agents](https://neon.com/use-cases/ai-agents)).
- [추론] agent는 틀린 시도를 자주 한다. "복사본에서 해 보고, 좋으면 반영하고, 아니면 버린다"가 싸면 agent 루프가 안전해진다.

### 다른 시스템
- [사실] Neon branch는 "copy-on-write clone"이고 즉시 만들어지며, 쓰기는 delta로 저장된다. history window 안의 어느 시점으로든 되돌릴 수 있다 ([Neon Branching](https://neon.com/docs/introduction/branching)).
- [사실] Dolt: "a SQL database you can fork, clone, branch, merge, push and pull just like a Git repository" ([Dolt docs](https://www.dolthub.com/docs/introduction/what-is-dolt)).
- [사실] PlanetScale 개발 branch는 스키마만 복사한다("only the schema is copied"). 변경은 deploy request로 리뷰 후 반영한다 ([PlanetScale Branching](https://planetscale.com/docs/vitess/schema-changes/branching)).
- [사실] Cloudflare D1 Time Travel: "restore a database to any minute within the last 30 days" ([D1](https://developers.cloudflare.com/d1/)).

### Oakdb 적합도: **중상**
- [추론] B+Tree를 COW로 만들면 "루트 페이지 번호 하나를 복사"하는 것으로 브랜치가 생긴다. 같은 구조가 crash recovery(새 루트를 원자적으로 기록)와 읽기 스냅샷(MVCC 읽기)에도 쓰이므로 핵심 목표와 맞물린다. 반대로 merge(브랜치 합치기)와 안 쓰는 페이지 회수(GC)까지 가면 범위를 넘는다.

### 난이도: 상
- [추론] "브랜치 생성 + 브랜치에서 쓰기 + 버리기/원본으로 교체(fast-forward)"까지만 하면 상~중상, 행 단위 3-way merge까지 하면 2개월로는 무리다.

---

## 3. 변경 구독 (CDC, change data capture)

**비유**: 택배 "배송 알림". 내가 계속 조회하지 않아도 상태가 바뀌면 알려 준다.

### SQLite 현재
- [사실] `sqlite3_update_hook()`은 **그 connection에** 콜백을 등록해 rowid table의 insert/update/delete 때 호출한다. WITHOUT ROWID table, 내부 table, ON CONFLICT REPLACE로 지워지는 행, truncate 최적화에서는 호출되지 않는다 ([update_hook](https://www.sqlite.org/c3ref/update_hook.html)).
- [사실] session extension은 변경을 changeset/patchset으로 묶어 다른 DB에 적용하거나 invert해서 undo할 수 있다. 기본으로 꺼져 있어 컴파일 옵션이 필요하고, PRIMARY KEY가 선언된 table만, virtual table은 안 된다 ([Session intro](https://www.sqlite.org/sessionintro.html)).
- [추론] 둘 다 같은 프로세스·connection 안에서만 동작한다. "다른 프로세스의 agent에게 변화를 밀어 준다"는 기능은 없다.

### 다른 시스템
- [사실] PostgreSQL `NOTIFY`: LISTEN한 클라이언트에 이벤트를 보내며, 트랜잭션 안의 NOTIFY는 커밋돼야만 전달된다. payload는 기본 8000바이트 미만 ([NOTIFY](https://www.postgresql.org/docs/current/sql-notify.html)).
- [사실] PostgreSQL logical decoding은 WAL을 해석해 변경을 내보내며, replication slot은 "crash-safe"하고 변경을 원래 순서대로 재생한다 ([Logical Decoding](https://www.postgresql.org/docs/current/logicaldecoding-explanation.html)).

### Oakdb 적합도: **중**
- [추론] WAL을 이미 만들기 때문에 "LSN(로그 위치) X 이후 커밋된 변경을 흘려 달라"는 API는 재사용 범위가 크다. crash 후에도 같은 위치부터 이어 받는다는 것을 보이면 recovery와도 엮인다. 다만 동시성·스토리지 실력을 보여 주는 정도는 1·2보다 작다.

### 난이도: 중

---

## 4. 네트워크 접근 (기존 후보 B)

### SQLite 현재
- [사실] "There really is no server." 프로세스가 DB 파일을 직접 읽고 쓴다. 서버형은 클라이언트 버그로부터 보호하고 "finer-grained locking and better concurrency"가 가능하다고 SQLite 스스로 쓴다 ([Serverless](https://www.sqlite.org/serverless.html)).
- [사실] "If there are many client programs sending SQL to the same database over a network, then use a client/server database engine instead of SQLite." 네트워크 파일시스템의 locking은 버그가 많아 손상될 수 있다 ([When To Use](https://www.sqlite.org/whentouse.html)). WAL은 공유 메모리가 필요해 "does not work over a network filesystem" ([WAL](https://www.sqlite.org/wal.html)).

### 다른 시스템
- [사실] Turso는 HTTP로 SQL을 실행하는 `POST /v2/pipeline` API를 제공한다 ([Turso HTTP reference](https://docs.turso.tech/sdk/http/reference)).
- [사실] Cloudflare D1은 "SQLite's SQL semantics"를 가진 관리형 DB로 Worker와 HTTP API로 접근한다 ([D1](https://developers.cloudflare.com/d1/)). 그런데 "Each individual D1 database is inherently single-threaded, and processes queries one at a time" ([D1 Limits](https://developers.cloudflare.com/d1/platform/limits/)). 즉 네트워크를 붙여도 single writer 문제는 그대로 남는다.

### Oakdb 적합도: **중**
- [추론] 이미 "최소 네트워크 서버 + 벤치마크"를 만들 계획이라 이건 **어차피 생기는 기능**이다. 두 개 뿐인 차별점 자리를 쓰기보다, 1번(동시 writer)을 **네트워크 너머에서 보여 주는 수단**으로 쓰는 편이 낫다. D1 사례가 "서버만 붙인다고 동시 쓰기가 해결되지 않는다"는 좋은 대비가 된다.

### 난이도: 하~중

---

## 5. 복제 (기존 후보 C)

- [사실] SQLite 코어에는 복제 기능이 없다. session extension이 changeset을 "another database with the same schema"에 적용하는 재료를 줄 뿐이다 ([Session intro](https://www.sqlite.org/sessionintro.html)).
- [사실] PostgreSQL은 logical decoding/replication slot으로 변경 스트림을 제공한다 ([Logical Decoding](https://www.postgresql.org/docs/current/logicaldecoding-explanation.html)).
- **Oakdb 적합도: 하**. [추론] 분산은 뒤로 미뤘다. 3번(CDC)을 만들어 두면 나중에 복제의 출발점이 된다.

---

## 6. 사용자·권한 (agent마다 다른 권한)

### SQLite 현재
- [사실] SQLite 3.48.0(2025-01-14) 릴리스 노트: "Remove vestigial traces of SQLITE_USER_AUTHENTICATION" ([Release History](https://www.sqlite.org/changes.html)). DB 안에 사용자·비밀번호를 두는 확장이 없어졌다.
- [사실] 대신 `sqlite3_set_authorizer()`가 있다. SQL을 compile할 때 동작마다 콜백을 불러 OK/IGNORE/DENY를 고른다. "preparing SQL statements from an untrusted source"에 쓰라고 하며, connection당 하나다 ([set_authorizer](https://www.sqlite.org/c3ref/set_authorizer.html)).

### 다른 시스템
- [사실] PostgreSQL은 `GRANT` 권한 체계에 더해 row security policy로 사용자별로 볼 수 있는·고칠 수 있는 행을 제한한다 ([PG Row Security](https://www.postgresql.org/docs/current/ddl-rowsecurity.html)).

### Oakdb 적합도: **하~중**
- [추론] 서버가 있어야 "누가 접속했나"가 의미가 있고, 엔진 핵심(스토리지·동시성)과 거리가 있다. 최소 서버에 "토큰별 read-only/read-write" 정도는 하루 작업이지만 차별점으로 내세울 만하지는 않다.

### 난이도: 중

---

## 7. 변경 이력·감사 로그 (temporal table)

**비유**: 은행 거래 내역. 잔액만이 아니라 "언제 얼마가 왜 바뀌었는지"가 남는다.

- [사실] MariaDB system-versioned table은 "store the history of all changes"이고 SQL:2011 표준에서 처음 도입됐다. `SELECT * FROM t FOR SYSTEM_TIME AS OF TIMESTAMP'...'`로 과거 모습을 조회한다 ([MariaDB System-Versioned Tables](https://mariadb.com/docs/server/reference/sql-structure/temporal-tables/system-versioned-tables)).
- [추론] SQLite에는 해당 기능이 없고, trigger로 history table을 직접 만들어야 한다. ([미검증] SQLite 문서에서 "temporal table 미지원"이라는 명시 문구를 찾지는 않았다. SQL 문법 문서에 `FOR SYSTEM_TIME`이 없다는 것으로 판단.)
- **Oakdb 적합도: 중**. [추론] MVCC 버전을 GC하지 않고 남기면 "AS OF" 읽기는 구조적으로 가깝다. 하지만 GC와 정면으로 부딪히고, 2번(스냅샷)과 겹친다. 2번을 고르면 일부를 함께 얻는다.
- 난이도: 중

---

## 8. 스키마 변경 (마이그레이션)

- [사실] SQLite `ALTER TABLE`은 "a limited subset"이다. table 이름 바꾸기, column 이름 바꾸기·추가·삭제, 그리고 3.53.0(2026-04-09)부터 NOT NULL 설정·해제가 된다. column 추가 시 PRIMARY KEY/UNIQUE 불가, 삭제 시 index·FK·trigger·view에 걸린 column은 불가다. 타입 변경 등 나머지는 새 table을 만들어 옮기는 **12단계 절차**를 정확히 따라야 한다 ([ALTER TABLE](https://www.sqlite.org/lang_altertable.html)).
- [추론] AI가 스키마를 자주 바꾸는 개발 흐름에서는 불편하지만, 이건 SQL layer 기능이다. Oakdb의 SQL은 최소로 두기로 했으므로 **적합도 하**. 난이도 중.

---

## 9. 관측성 (쿼리 통계)

- [사실] 각 prepared statement는 `SQLITE_STMTSTATUS` 카운터를 갖고, table step 수가 검색·결과 행 수보다 훨씬 많으면 full table scan을 의심할 수 있다고 문서가 설명한다 ([stmt_status](https://www.sqlite.org/c3ref/stmt_status.html)).
- [추론] PostgreSQL처럼 서버 전체 통계 뷰는 없지만 기본 재료는 있다. 큰 gap이 아니다. **적합도 하**. 다만 Oakdb 서버에 "충돌·abort 횟수, 커밋 대기 시간" 카운터를 노출하는 것은 1번의 증거물로 쓸 만하다.

---

## 10. AI가 만든 쿼리의 안전장치 (거짓 gap 주의)

SQLite가 **이미 할 수 있는 것**이다. 차별점으로 고르면 안 된다.

- [사실] SQLite 보안 문서는 신뢰할 수 없는 SQL을 받을 때 `SQLITE_DBCONFIG_DEFENSIVE`, `sqlite3_limit()`로 한도 낮추기(표로 권장값 제공), authorizer로 CREATE/DROP 막기, progress handler나 `sqlite3_interrupt()`로 오래 도는 쿼리 끊기, `sqlite3_hard_heap_limit64()`로 메모리 상한 걸기를 권한다 ([Security](https://www.sqlite.org/security.html)).
- [사실] progress handler 콜백이 0이 아닌 값을 돌려주면 실행 중인 연산이 중단된다 ([progress_handler](https://www.sqlite.org/c3ref/progress_handler.html)).
- [사실] `PRAGMA query_only`를 켜면 CREATE/DELETE/DROP/INSERT/UPDATE가 `SQLITE_READONLY` 오류가 된다 ([PRAGMA](https://www.sqlite.org/pragma.html)).

---

## 11. 벡터 검색 (범위 밖)

- [사실] `sqlite-vec`은 "An extremely small, 'fast enough' vector search SQLite extension"이고 pre-v1이라 "expect breaking changes!"라고 적혀 있다 ([sqlite-vec](https://github.com/asg017/sqlite-vec)). 코어 기능이 아니라 확장이다.
- 사용자가 벡터를 의도적으로 제외했으므로 **범위 밖**으로 둔다.

---

## 추천 2개

### 사실만 다시 모으면
- SQLite는 writer가 하나이고, 이를 푸는 `BEGIN CONCURRENT`는 trunk가 아닌 branch에 있으며 page 단위로 충돌을 판정한다.
- SQLite의 스냅샷 수단은 전체 복사(backup API, `VACUUM INTO`)이거나, 컴파일 옵션이 필요한 읽기 전용 과거 스냅샷이다.
- Turso는 single writer를 "show its age"라며 행 단위 MVCC를 만들고 있다. Neon은 DB 80% 이상을 agent가 만든다고 하고, agent용으로 branching·snapshot을 내세운다.
- SQLite에 네트워크를 붙인 D1도 DB당 한 번에 쿼리 하나다.

### [권고] 1순위: 동시 writer (key 단위 충돌 판정)
- 이유 1 [추론]: 프로젝트 핵심 목표(동시성 + 트랜잭션)를 그대로 보여 준다.
- 이유 2 [추론]: SQLite 자신이 문서에서 약점이라고 인정하고, 비교 대상(page 단위 `BEGIN CONCURRENT`)이 분명해서 "같은 page에 몰린 서로 다른 key"를 여러 writer가 갱신하는 벤치마크 하나로 차이를 숫자로 보일 수 있다.
- 이유 3 [추론]: AI agent 여러 개가 같은 DB에 동시에 쓰는 이야기로 블로그 서사를 만들기 쉽다.
- 범위 제안: 실행은 병렬, 커밋은 직렬화(SQLite·Turso와 같은 골격), 충돌 판정은 key 단위 read/write set 비교.

### [권고] 2순위: 싼 스냅샷·브랜치 (COW B+Tree, "시도하고 버리기")
- 이유 1 [추론]: B+Tree를 copy-on-write로 만들면 브랜치, 읽기 스냅샷(MVCC 읽기), crash 시 원자적 루트 교체가 **같은 구조**에서 나온다. 스토리지 + recovery 목표와 맞물린다.
- 이유 2 [사실+추론]: Neon이 agent 사용을 1차 근거로 브랜치·스냅샷을 내세우고 있어, "AI 시대에 왜"를 출처로 받칠 수 있다. SQLite는 전체 복사밖에 없어 대비가 분명하다.
- 범위 제안: 브랜치 생성, 브랜치에서 쓰기, 버리기, 원본이 그동안 바뀌지 않았을 때만 원본으로 교체(fast-forward). 행 단위 merge와 정교한 페이지 GC는 범위 밖.
- 위험 [추론]: 1순위와 같이 하면 "여러 writer + COW 루트 교체"를 맞춰야 한다. 커밋을 직렬화하는 1순위 설계라면 루트 교체도 커밋 경로에서 한 번만 일어나므로 충돌하지 않는다. 2개월 안에 버겁다고 판단되면 **3번(WAL 기반 변경 구독)**으로 바꾸는 것이 난이도가 낮은 대안이다.

### 고르지 않은 이유
- 네트워크 접근(B): 어차피 최소 서버로 만든다. 차별점이 아니라 1순위를 원격에서 보여 주는 무대로 쓴다.
- 복제(C): 분산은 범위 밖.
- 권한·스키마 변경·관측성: 엔진 핵심과 거리가 있거나 SQLite도 이미 어느 정도 한다.
- 쿼리 안전장치: SQLite가 이미 한다(거짓 gap).

---

## 미검증
- Databricks "State of AI Agents" 보고서의 "AI agents create 97% of database branches on Neon" 수치는 검색 요약에서만 봤고 원문을 직접 확인하지 않았다. 본문에서는 인용하지 않았다.
- Turso의 동시 쓰기 성능 수치(최대 4배, 7배)와 성숙도(beta/production-ready 여부)는 블로그·manual 간 표현이 달라 인용하지 않았다.
- SQLite 문서에 "temporal table을 지원하지 않는다"는 명시 문구는 찾지 않았다(문법 문서에 해당 구문이 없다는 것으로 판단).
- `SQLITE_STMTSTATUS_*` 각 카운터 이름은 상수 페이지(`c_stmtstatus_counter.html`)를 직접 열어 확인하지 않았다.
- "AI agent가 같은 DB에 동시에 쓰는 일이 실제로 얼마나 흔한가"를 보여 주는 1차 통계는 찾지 못했다. 1번의 AI 관련 근거는 [추론]이다.
- COW B+Tree와 key 단위 OCC를 함께 2개월 안에 구현할 수 있다는 판단은 [추론]이다.

---

## 출처

**SQLite**
- When To Use: https://www.sqlite.org/whentouse.html
- Write-Ahead Logging: https://www.sqlite.org/wal.html
- BEGIN CONCURRENT (begin-concurrent branch 문서): https://www.sqlite.org/src/doc/begin-concurrent/doc/begin_concurrent.md
- begin-concurrent branch timeline: https://www.sqlite.org/src/timeline?r=begin-concurrent&n=5
- Serverless: https://www.sqlite.org/serverless.html
- ALTER TABLE: https://www.sqlite.org/lang_altertable.html
- update_hook: https://www.sqlite.org/c3ref/update_hook.html
- Session extension: https://www.sqlite.org/sessionintro.html
- Online Backup API: https://www.sqlite.org/backup.html
- VACUUM (INTO): https://www.sqlite.org/lang_vacuum.html
- snapshot_open: https://www.sqlite.org/c3ref/snapshot_open.html
- set_authorizer: https://www.sqlite.org/c3ref/set_authorizer.html
- Security (untrusted SQL): https://www.sqlite.org/security.html
- progress_handler: https://www.sqlite.org/c3ref/progress_handler.html
- PRAGMA (query_only): https://www.sqlite.org/pragma.html
- stmt_status: https://www.sqlite.org/c3ref/stmt_status.html
- Release History (3.48.0): https://www.sqlite.org/changes.html
- sqlite-vec: https://github.com/asg017/sqlite-vec

**Turso / Cloudflare D1**
- Turso blog, Beyond the Single-Writer Limitation (2025-10-06): https://turso.tech/blog/beyond-the-single-writer-limitation-with-tursos-concurrent-writes
- Turso Concurrent Writes docs: https://docs.turso.tech/tursodb/concurrent-writes
- Turso HTTP API reference: https://docs.turso.tech/sdk/http/reference
- Cloudflare D1: https://developers.cloudflare.com/d1/
- Cloudflare D1 Limits: https://developers.cloudflare.com/d1/platform/limits/

**브랜칭 / 버전 관리**
- Neon Branching: https://neon.com/docs/introduction/branching
- Neon for AI Agents: https://neon.com/use-cases/ai-agents
- Neon blog, Neon and Databricks (2025-05-14): https://neon.com/blog/neon-and-databricks
- Dolt, What is Dolt: https://www.dolthub.com/docs/introduction/what-is-dolt
- PlanetScale Branching: https://planetscale.com/docs/vitess/schema-changes/branching

**PostgreSQL / MariaDB**
- PostgreSQL MVCC Introduction: https://www.postgresql.org/docs/current/mvcc-intro.html
- PostgreSQL NOTIFY: https://www.postgresql.org/docs/current/sql-notify.html
- PostgreSQL Logical Decoding Concepts: https://www.postgresql.org/docs/current/logicaldecoding-explanation.html
- PostgreSQL Row Security Policies: https://www.postgresql.org/docs/current/ddl-rowsecurity.html
- MariaDB System-Versioned Tables: https://mariadb.com/docs/server/reference/sql-structure/temporal-tables/system-versioned-tables

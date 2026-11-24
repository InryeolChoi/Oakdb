# 0023. SQL 트랜잭션 명령 — 회고

이슈 #35. 설계는 `docs/design/0023-sql-transactions.md`. 사용자는 T1~T13과 Q1~Q10을 모두 추천안대로 정했다.

## 만든 것

- **세션 (`Session<'db, F>`)**: `&Db`를 빌리고 `Option<Txn>`을 들고 있다. `Session::new(&db)`, `execute(&mut self, sql)`, `in_transaction()` 셋이 공개 API의 전부다. #37에서는 접속 스레드가 `serve()` 안에서 세션 하나를 만들면 된다.
- **문법**: `BEGIN`, `COMMIT`, `ROLLBACK`, 그리고 각각 뒤에 붙는 선택 낱말 `TRANSACTION`. `BEGIN`·`COMMIT`·`ROLLBACK`은 예약어가 되었고 `TRANSACTION`은 아니다. `START TRANSACTION`, `END`, `BEGIN IMMEDIATE`, `ROLLBACK TO`, `SAVEPOINT`는 "지원 안 함"이다.
- **파서가 내는 모양**: `Command { Begin, Commit, Rollback, Run(Statement) }`. 실행기 `apply`는 `Statement`만 받으므로 "BEGIN을 실행한다"는 길이 타입에 없다.
- **상태 규칙**: 트랜잭션을 끝내는 것은 COMMIT과 ROLLBACK뿐이다. COMMIT은 실패해도(`Conflict`, `OutcomeUnknown`, `Stopped`) 트랜잭션을 끝낸다. 트랜잭션 중 BEGIN, 트랜잭션 밖 COMMIT/ROLLBACK은 새 오류 `SqlError::Transaction(TransactionError::…)`가 되고, 열린 트랜잭션은 그대로 남는다.
- **INSERT를 두 단계로**: 모든 행의 타입, 중복(표와 같은 문장 안), 키·값 길이를 먼저 확인하고, 그다음 모두 쓴다. 이제 실행기의 다섯 문장이 모두 "검사 먼저, 쓰기 나중"이다.
- **한 번에 하나 실행하는 `execute(&db, sql)`**: 남겨 두었다. BEGIN/COMMIT/ROLLBACK을 받으면 `NeedsSession` 오류를 낸다.
- **충돌 메시지**: 오류 종류는 그대로 `Engine(Conflict)`, 서버 코드도 `0x20`이다. 메시지에만 "BEGIN으로 시작했다면 BEGIN부터 다시"를 덧붙였다.
- **테스트 28개 추가**:
  - 통합 테스트 27개 (`tests/transactions.rs`).
  - 모델 비교 속성 테스트 1개 (`tests/txn_model.rs`): 200개 사례, 사례마다 BEGIN·COMMIT·ROLLBACK·INSERT·UPDATE·DELETE를 최대 39개 이어 돌린다.

| 검증 | 결과 |
|---|---|
| 실패하는 테스트 먼저 | a378099. `Session`과 `TransactionError`가 아직 없어 새 테스트 파일 둘이 컴파일되지 않았다 |
| 모델이 매 문장 확인하는 것 | ① 결과나 오류 종류, ② `in_transaction()`, ③ 세션이 보는 표 전체, ④ 다른 트랜잭션에서 읽은 커밋된 표 전체. 오류가 난 문장 뒤의 표는 문장 전과 같아야 한다 |
| 기존 테스트 | "BEGIN/COMMIT/ROLLBACK은 지원 안 함"을 확인하던 줄만 `VACUUM`, `START TRANSACTION`, `SAVEPOINT a`로 바꿨다(사용자 허락). `tests/sql.rs`, `tests/blind_sql.rs`에 있다. 서버 테스트에는 그런 줄이 없었다 |
| 리뷰·눈 가린 테스터·장애 실험 | PR에서 검증 대기 |
| A/B | 해당 없음. 성능 주장이 없다 |

## 배운 것

**구현의 대부분은 이미 앞 티켓들이 해 두었다.** 엔진의 `Txn`은 처음부터 스냅샷과 자기 쓰기를 합쳐 읽었고(0015), `Drop`에서 쓰기를 버렸으며(0016), 0022는 `apply(&mut Txn, Statement)`를 따로 꺼내 두었다. 세션이 하는 일은 "커밋을 지금 하나, COMMIT 때 하나"를 고르는 것뿐이다. 세션 본체는 주석을 빼면 40줄 남짓이다. 쇼핑몰로 치면 장바구니(쓰기 모음)와 결제(커밋)는 이미 있었고, 이번에 만든 것은 "장바구니를 들고 다니는 손님"이다.

**Rust의 소유권이 상태 규칙을 대신 정해 줬다.** 엔진의 `Txn::commit(self)`는 트랜잭션을 가져가 버린다. 그래서 "COMMIT이 실패한 뒤에도 트랜잭션이 남아 있나?"라는 질문에는 답이 하나뿐이다. 남을 수가 없다. 세션은 `Option::take()`로 트랜잭션을 꺼낸 다음 커밋하므로, 성공이든 실패든 세션은 자동 커밋 상태로 돌아간다. 이 규칙을 따로 코드로 지킬 필요가 없었다. Java의 JDBC에서는 `commit()`이 예외를 던진 뒤 그 `Connection`이 어떤 상태인지 드라이버마다 다르다.

**세션이 `Db`를 빌리게 한 것은 #37을 위한 선택이었다.** `Arc<Db>`를 갖게 하면 구조체 안에서 `Txn`이 옆 칸의 `Db`를 빌리는 자기 참조가 되어, 안전한 Rust로는 쓸 수 없다. 그런데 서버의 접속 스레드는 이미 `Arc<Shared>`를 끝까지 쥐고 있다. 그래서 그 안에서 `&shared.db`를 빌리면 수명 표시(`'db`) 하나로 모든 것이 맞는다. 세션은 그 스레드의 스택에서 태어나고, 그 스택에서 버려진다. 접속이 끊기든 스레드가 패닉하든 되감기 중에 `Drop`이 불려 롤백된다.

**"문장 하나만 취소"는 세이브포인트 없이 얻었다.** 0022에서 UPDATE를 "모아서 검사하고, 그다음 쓰기"로 만들어 둔 덕분에 남은 것은 INSERT 하나였다. 그런데 INSERT에는 작은 함정이 있었다. 예전에는 같은 문장 안의 중복을 "트랜잭션이 자기 쓰기를 본다"에 기대어 잡았다. 첫 행을 이미 썼으니 둘째 행의 `get`이 그것을 찾았다. 쓰기를 뒤로 미루자 그 기댈 곳이 사라졌다. 그래서 문장 안의 키 집합(`HashSet`)을 따로 둔다. 검사 순서(값 개수 → 타입 → 키 길이 → 중복 → 값 길이)는 예전과 같게 두어, 첫 오류의 종류가 바뀌지 않게 했다.

**동시에 CREATE TABLE하면 서로 충돌한다는 사실을 테스트로 고정했다.** 표 번호 카운터가 키 하나라서, 이름이 다른 표라도 두 트랜잭션이 함께 만들면 나중 COMMIT이 `Conflict`가 된다. 설계에서 받아들인 동작이므로, 나중에 누군가 카운터를 바꾸면 이 테스트가 그 변화를 알려 준다. 같은 이름이면 표 정보 키에서도 충돌한다. 이 충돌 덕분에 같은 이름의 표가 둘 생기지 않는다.

**Rust 쪽에서 눈여겨본 점**

- **`Session<'db, F: Fs = StdFs>`**: 엔진의 `Txn`과 같은 기본 타입 인자를 써서, 보통 사용자는 `Session<'_>`만 쓰면 된다. 테스트는 `Session<'_, SimFs>`를 쓴다.
- **`&mut self`**: 한 세션을 두 스레드가 함께 쓰는 일을 컴파일러가 막는다. 동시성은 세션을 여러 개 만들어 얻는다. 여러 스레드 테스트는 `thread::scope`로 `&db`를 빌려 주고, 스레드마다 세션을 하나씩 만든다.
- **`#[cfg(test)] fn parse`**: 이제 제품 코드는 `parse_command`만 쓴다. 기존 단위 테스트가 쓰던 `parse`는 테스트 빌드에만 남겼다. 그래서 쓰이지 않는 코드라는 경고가 나지 않는다.

## 남긴 것

- 서버가 세션을 쓰는 일, `SqlError::Transaction`의 서버 오류 코드, docs/protocol.md의 "QUERY 하나 = 트랜잭션 하나" 문장은 #37에서 한다. 그전까지 서버로 BEGIN을 보내면 `0x1F`(위에 없는 문장 오류)와 "세션이 필요하다"는 메시지가 간다.
- 오래 열린 트랜잭션은 BEGIN 때의 트리와 충돌 기록을 붙잡는다. 시간 제한은 #37 이후로 미뤘다.
- 오류 종류의 이름 정리는 #34가 맡는다.

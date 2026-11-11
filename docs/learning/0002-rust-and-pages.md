# Oakdb 코드를 읽기 위한 Rust 문법, 그리고 페이지

> Oakdb 학습 노트 #2 (2026-10-12). 01번(한 장짜리 저장)을 끝낸 뒤, 코드를 읽는 데 필요한 Rust 문법과 "페이지"라는 단위를 정리했다. Java/Spring 개발자 입장에서 비교했다.

## 1부. 필요한 Rust 문법만

### ① `struct` + `impl` = Java의 class

Rust는 데이터(struct)와 메서드(impl)를 따로 적는다.

```rust
pub struct Db<F: Fs = StdFs> {   // 필드만
    file: F::File,
    meta: Meta,
    leaf: Mutex<LeafPage>,
}

impl<F: Fs> Db<F> {              // 메서드는 여기
    pub fn begin(&self) -> Txn<'_, F> { ... }
}
```

| Rust | Java |
|---|---|
| `pub` | `public` |
| 아무것도 안 붙임 | `private` (기본이 비공개) |
| `pub(crate)` | 이 crate 안에서만 (패키지 전용과 비슷) |
| `Db::open(...)` | static 메서드 |
| `db.begin()` | 인스턴스 메서드 |

### ② `trait` = Java의 interface

```rust
pub trait Fs {
    type File: FsFile;                           // 연관 타입
    fn open(&self, path: &Path) -> io::Result<Self::File>;
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;
}

impl Fs for StdFs { ... }      // 진짜 파일시스템
impl Fs for NoSyncFs { ... }   // 벤치마크용: sync를 안 함
```

`type File: FsFile`(연관 타입)은 "이 파일시스템이 여는 파일의 종류는 구현하는 쪽이 정한다"는 뜻이다. 엔진은 `Fs`만 알고 진짜 디스크는 모른다. 그래서 테스트에서 가짜 파일시스템을 끼워 정전을 흉내 낼 수 있다. Spring에서 인터페이스에 Mock을 주입하는 것과 같다.

### ③ 제네릭과 기본 타입

```rust
pub struct Db<F: Fs = StdFs>
```

- `F: Fs`: Java의 `<F extends Fs>`
- `= StdFs`: 안 적으면 `StdFs`. 사용자는 `Db`만 쓰고, 테스트만 `Db<RecordingFs>`로 바꿔 끼운다.

Java와 달리 Rust 제네릭은 컴파일할 때 타입마다 코드를 따로 만든다. 실행 중에 "어느 구현이지?"를 찾는 비용이 없다.

### ④ 소유권과 빌림 (Rust의 핵심)

Java는 가비지 컬렉터가 메모리를 치운다. Rust는 "값의 주인은 하나"라는 규칙으로 컴파일러가 치울 때를 정한다.

| 표기 | 뜻 | 비유 |
|---|---|---|
| `Vec<u8>` | 바이트 배열을 **소유** | 내 책 |
| `&[u8]` | **빌려서 읽기만** | 도서관에서 열람 |
| `&mut self` | **빌려서 고치기** (그동안 남은 못 봄) | 빌린 책에 메모 |
| `self` | **넘겨받아 다 씀** | 책 기증 |

```rust
pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
    self.writes.insert(key.to_vec(), value.to_vec());  // 빌린 걸 복사해 내 것으로
    Ok(())
}

pub fn commit(self) -> Result<()> { ... }   // 트랜잭션을 다 써 버림
pub fn rollback(self) {}                    // 몸통이 비어 있다
```

- `commit(self)`: 커밋하면 트랜잭션이 사라진다. 커밋한 트랜잭션을 또 쓰면 **컴파일 에러**다. Java라면 실행 중 `IllegalStateException`으로 잡았을 실수다.
- `rollback(self) {}`가 비어 있는 이유: 넘겨받은 트랜잭션은 함수가 끝나면 자동으로 치워진다(drop). 쓰기 버퍼도 같이 사라지니 그게 롤백이다.

"여러 명이 읽기" 또는 "한 명만 고치기" 중 하나만 허용되고, 컴파일러가 이를 검사해서 데이터 경쟁을 막는다.

### ⑤ 수명 `'db`

```rust
pub struct Txn<'db, F: Fs = StdFs> {
    db: &'db Db<F>,
    writes: BTreeMap<Vec<u8>, Vec<u8>>,
}
```

"이 트랜잭션은 빌린 `Db`보다 오래 살 수 없다"는 표시다.

```rust
let txn = db.begin();
db.close();      // ❌ 컴파일 에러: txn이 아직 db를 빌리고 있음
txn.commit();
```

열린 트랜잭션을 두고 DB를 닫는 버그를 컴파일러가 막는다. `begin(&self) -> Txn<'_, F>`의 `'_`는 "수명은 알아서 채워 줘"라는 뜻이다.

### ⑥ 오류 처리: `Result`, `?`, `enum`

Rust에는 예외가 없다. 실패할 수 있는 함수는 `Result`를 돌려준다.

```rust
pub enum Error {
    Io(io::Error),
    KeyTooLarge { len: usize, max: usize },  // 값을 품은 enum
    PageFull,
    AlreadyOpen,
    Corrupt(String),
}

fs.create_dir_all(dir)?;   // ? = 실패면 바로 return Err
```

| Rust | Java |
|---|---|
| `Result<T, E>` | `T`를 돌려주거나 `throws E` |
| `?` | 예외 전파 (단, 눈에 보이게) |
| 데이터를 품은 `enum` | 예외 클래스마다 필드 |
| `match err { Error::PageFull => ... }` | `catch (PageFullException e)` |
| `Option<T>` | `Optional<T>` (그리고 null이 아예 없다) |

- `impl From<io::Error> for Error`: io 오류가 `?`를 지나면 자동으로 `Error::Io`가 된다.
- `#[non_exhaustive]`: 오류 종류를 나중에 더해도 사용자 코드가 깨지지 않는다. 실제로 `AlreadyOpen`을 더했다.

### ⑦ `Mutex`: 잠금이 데이터를 감싼다

```rust
leaf: Mutex<LeafPage>,

let mut leaf = self.db.leaf.lock().unwrap();  // 잠그고 접근권 받기
*leaf = next;
// 함수 끝 → 자동 해제
```

Java의 `synchronized`는 잠금과 데이터가 따로라서 잠그지 않고 만지는 실수가 가능하다. Rust는 데이터가 잠금 안에 들어 있어서 **잠그지 않으면 꺼낼 수조차 없다.**

### ⑧ 바이트 다루기

```rust
buf: Box<[u8]>                                             // 힙의 고정 길이 배열
self.buf[at..at + 2].copy_from_slice(&v.to_le_bytes());   // 숫자 → 2바이트
u16::from_le_bytes([self.buf[at], self.buf[at + 1]])      // 2바이트 → 숫자
```

범위를 벗어나면 패닉이 난다. C처럼 쓰레기 메모리를 읽는 일은 없다.

### ⑨ OS별 코드: `#[cfg]`

```rust
#[cfg(target_os = "macos")]
fn data_sync(file: &File) -> io::Result<()> { full_sync(file) }   // F_FULLFSYNC

#[cfg(target_os = "linux")]
fn data_sync(file: &File) -> io::Result<()> { file.sync_data() }  // fdatasync
```

컴파일할 때 OS에 맞는 쪽만 남는다. 실행 중 분기가 없다.

### ⑩ `unsafe`

```rust
// SAFETY: the descriptor is valid for as long as `file` is borrowed.
let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) };
```

C 함수를 직접 부르면 컴파일러가 안전을 보장하지 못한다. `unsafe`로 표시하고, 왜 안전한지 `SAFETY:` 주석으로 적는다.

### ⑪ 테스트는 같은 파일에

```rust
#[cfg(test)]
mod tests {
    use super::*;     // 비공개 항목까지 보인다
    #[test]
    fn an_empty_leaf_has_all_space_free() { ... }
}
```

단위 테스트는 같은 파일, 엔진 API 통합 테스트는 `tests/` 폴더에 둔다.

## 2부. 페이지

### 한 줄 정의

**페이지는 DB가 디스크를 읽고 쓰는 고정 크기 단위다.** Oakdb는 4KB(4,096바이트).

### 왜 단위를 정하나

디스크는 1바이트씩 주지 않고 최소 단위로 통째로 읽고 쓴다. 볼펜 한 자루를 주문해도 상자 하나가 오는 택배와 같다. 어차피 상자째 오니 **상자를 꽉 채워 쓰자**는 것이 페이지 설계의 출발점이다.

### 파일은 페이지를 일렬로 붙인 것

```
oakdb.data
┌─────────┬─────────┬─────────┬─────────┐
│ 페이지 0 │ 페이지 1 │ 페이지 2 │ 페이지 3 │ ...
│ 메타 A   │ 메타 B   │ 리프     │ (나중에) │
└─────────┴─────────┴─────────┴─────────┘
0        4096      8192     12288
```

위치 = 번호 × 4096. 책에서 "37쪽"이면 바로 펼치는 것과 같다.

| 종류 | 비유 | 들어 있는 것 | 언제 |
|---|---|---|---|
| 메타 | 표지 | Oakdb 파일 표시, 페이지 크기, 루트 번호 | 01번 |
| 리프 | 본문 | 실제 키·값 | 01번 (한 장) |
| 내부 | 목차 | "키 m 이전은 5쪽, 이후는 9쪽" | 03~04번 |
| 오버플로 | 부록 | 한 장에 안 들어가는 큰 값의 나머지 | 06번 |

### 리프 페이지 안: 공책 한 장

```
┌──────────────────────────────┐ 0
│ 머리 (40바이트)                │
├──────────────────────────────┤ 40
│ 슬롯 → 위에서 아래로            │  [위치, 길이] × n   ← 키 순서
│        ( 빈 칸 )              │
│ 셀 ← 아래에서 위로              │  [키 길이|값 길이|키|값]
└──────────────────────────────┘ 4096
```

- 양쪽 끝에서 채우니 "목차는 몇 칸"을 미리 정할 필요가 없다. 가운데가 만나면 꽉 찬 것이다.
- 새 키를 중간에 끼울 때 4바이트짜리 슬롯만 한 칸씩 민다. 셀은 그대로 둔다.
- 슬롯이 키 순서라 이진 탐색으로 찾는다. 253개 중 하나를 8번 비교로 찾는다(측정 78ns).

### ft_malloc, 운영체제의 페이지와 비교

같은 아이디어가 세 군데에서 쓰인다.

| | ft_malloc | 운영체제 (가상 메모리) | Oakdb |
|---|---|---|---|
| 담는 것 | 사용자에게 내줄 메모리 블록 | 프로세스의 메모리 | 키-값 셀 |
| 크기 | `getpagesize()`의 배수 | 4KB (Apple Silicon Mac은 16KB) | 4KB |
| 받아 오는 곳 | `mmap` | 물리 메모리 | 파일 |
| 위치 찾기 | 존 주소 + 오프셋 | 페이지 테이블: 가상 번호 → 물리 프레임 | 번호 × 4096 |
| 머리 | 존 헤더 + 블록 헤더(크기, 사용 여부) | 페이지 테이블 항목(존재, dirty 비트) | 페이지 헤더 + 셀의 키·값 길이 |
| 조각 | free한 블록 사이 틈 | 외부 단편화 | 덮어쓴 셀이 남긴 틈 |

**결정적인 차이: 옮길 수 있느냐.**

ft_malloc은 블록을 옮길 수 없다. 사용자에게 이미 주소를 건넸기 때문이다. 옮기면 그 포인터가 엉뚱한 곳을 가리킨다. 그래서 조각은 옆에 붙은 빈 블록끼리 합치는(coalescing) 것으로만 줄인다.

Oakdb는 셀을 마음대로 옮긴다. 바깥은 셀 위치를 직접 쥐지 않고 **슬롯을 거쳐서** 찾아가기 때문이다.

```
ft_malloc:  사용자 포인터 ──────────→ 블록     (옮기면 깨짐)
Oakdb:      키 → 슬롯 [위치, 길이] ──→ 셀     (옮기고 슬롯만 고치면 끝)
```

그래서 페이지 정리(compact)가 가능하다. 살아 있는 셀을 끝으로 몰고 슬롯의 위치만 고치면 흩어진 틈이 한 덩어리가 된다.

```
덮어쓰기 전:   [빈칸 ........][a=old]
덮어쓴 뒤:     [빈칸 ....][a=new][~~old~~]
수백 번 뒤:    [][a=new][~~~][~~~][~~~]...   ← 꽉 참 (01번에서 찾은 버그)
정리 후:       [빈칸 ..........][a=new]
```

슬롯 배열은 간접 참조를 한 단계 끼워서 "옮겨도 되는 자유"를 사는 장치다. 가상 주소는 그대로 두고 물리 위치를 바꾸는 운영체제의 페이지 테이블과 같은 발상이다.

### 운영체제 책과 이어지는 것

| OS 개념 | Oakdb에서 |
|---|---|
| 페이지 캐시 | 버퍼 풀: 자주 쓰는 페이지를 메모리에 둔다 |
| 페이지 폴트 | 버퍼 풀에 없는 페이지를 파일에서 읽어 온다 |
| dirty 비트 | 고쳤지만 아직 디스크에 안 쓴 페이지 |
| LRU 교체 | 버퍼 풀이 차면 무엇을 내보낼지 |
| `fsync` | 커밋마다 하는 것. 커밋 시간의 99.9% |

Oakdb가 4KB인 이유는 OS 페이지 크기 때문이 아니다. SQLite의 기본값이고 SSD가 한 번에 쓰는 단위와 잘 맞기 때문이다(ADR 0008). 16KB 페이지를 쓰는 Mac에서도 OS 페이지 하나에 DB 페이지 네 장이 들어갈 뿐이라 문제가 없다.

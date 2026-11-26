# macOS와 Linux를 모두 지원

Oakdb는 macOS와 Linux 양쪽에서 빌드되고, 테스트를 통과하고, 같은 내구성 약속을 지켜야 한다. 개발은 macOS에서 하지만, 시스템 소프트웨어는 대부분 Linux 서버에서 돌기 때문에 한쪽만 지원하면 반쪽짜리 증거가 된다.

## Consequences

- 디스크에 확실히 기록하는 방법이 OS마다 다르다. macOS의 `fsync`는 드라이브 캐시까지 비우지 않아서 `F_FULLFSYNC`를 따로 써야 하고, Linux는 `fdatasync`/`fsync`를 쓴다. 이 차이는 파일 I/O를 모은 한 곳에서만 분기하고, 엔진의 나머지 코드는 OS를 몰라야 한다.
- 두 OS 모두에서 테스트를 돌려 차이를 일찍 잡는다.

## 구현 메모 (#59, PR #60)

- **rename도 두 OS에서 같게 durable하다**: 열기가 새 WAL을 rename으로 바꿔 끼운 뒤 디렉터리를 sync한다. 디렉터리 sync는 macOS에서 `F_FULLFSYNC`, Linux에서 `fsync`다(`fs.rs`의 `full_sync`). POSIX rename은 두 OS 모두 원자적이다. CI의 `fs_contract`는 동작이 같은지만 보고, 정전 뒤 남는지는 가짜 디스크의 테스트가 본다.

## 구현 메모 (#38 정상 종료)

- **신호는 `sigwait`, 두 OS가 같은 코드**: SIGINT·SIGTERM을 `pthread_sigmask`로 막고(`Db::open`보다, 즉 커밋 스레드보다 먼저) 전용 스레드가 `sigwait`로 받는다. POSIX라 macOS·Linux가 같고 `libc`만 쓴다. Linux의 `signalfd`, macOS의 `kqueue EVFILT_SIGNAL`은 쓰지 않는다. 신호 코드는 `#[cfg(unix)]`.
- **macOS의 논블로킹 상속**: 논블로킹 listener에서 `accept`한 소켓이 macOS에서는 논블로킹을 물려받는다(Linux는 아님). 종료 때 대기열을 비우는 곳에서 받은 소켓마다 `set_nonblocking(false)`를 부른다. 안 그러면 macOS에서만 `0x49` 뒤의 기다림(linger)이 바로 끝나 연결이 리셋될 수 있다.
- **listen 소켓의 `shutdown()`은 쓰지 않는다**: 잠든 `accept`를 깨우는 동작이 두 OS에서 다르다. 대신 자기 자신에게 루프백으로 접속해 깨운다.

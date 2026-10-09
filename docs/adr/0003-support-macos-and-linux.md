# macOS와 Linux를 모두 지원

Oakdb는 macOS와 Linux 양쪽에서 빌드되고, 테스트를 통과하고, 같은 내구성 약속을 지켜야 한다. 개발은 macOS에서 하지만, 시스템 소프트웨어는 대부분 Linux 서버에서 돌기 때문에 한쪽만 지원하면 반쪽짜리 증거가 된다.

## Consequences

- 디스크에 확실히 기록하는 방법이 OS마다 다르다. macOS의 `fsync`는 드라이브 캐시까지 비우지 않아서 `F_FULLFSYNC`를 따로 써야 하고, Linux는 `fdatasync`/`fsync`를 쓴다. 이 차이는 파일 I/O를 모은 한 곳에서만 분기하고, 엔진의 나머지 코드는 OS를 몰라야 한다.
- 두 OS 모두에서 테스트를 돌려 차이를 일찍 잡는다.

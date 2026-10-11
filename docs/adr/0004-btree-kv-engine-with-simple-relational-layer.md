# B+Tree 키-값 엔진 위의 단순한 관계형 DB로 범위를 좁힘

처음에는 relational, document, vector를 모두 지원하는 DB를 장기 목표로 잡았지만, 세 가지를 동시에 노리면 어느 것도 깊이 있게 만들 수 없다고 판단했다. Oakdb는 **B+Tree 기반 키-값 엔진 위에 단순한 관계형(RDB) 구조를 얹은 DB**로 범위를 좁힌다. SQLite와 같은 기본 구조이며, 대신 SQLite가 못 하는 지점 두 개를 골라 그것을 살리는 데 집중한다.

## Considered Options

- **LSM-Tree**: 쓰기가 빠르고 동시성·복구 추론이 단순해서 조사에서는 추천되었다(`docs/research/0001`). 하지만 이 프로젝트는 "간단한 RDB 구조"를 목표로 하므로, 관계형 DB의 전통적인 구조이자 SQLite와 직접 비교할 수 있는 B+Tree를 택했다.
- **document, vector 모델**: 이번 범위에서 제외한다.

## Consequences

- ADR 0002의 "장기적으로 relational/document/vector 지원" 문구는 이 결정으로 대체된다.
- B+Tree를 제자리에서 수정하므로 페이지 단위 동시성 제어와 복구(정전 시 반쯤 쓴 페이지 처리)를 직접 설계해야 한다. 2개월 안에서 가장 어려운 부분이 될 것이다.

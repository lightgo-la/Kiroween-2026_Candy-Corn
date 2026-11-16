# Compiler Rules

이 파일의 `- ` 불릿은 Candy-Corn이 제약 AST(ConstraintSubAST)로 직접 파싱해 주입한다.
(`--steering` CLI 옵션 또는 GUI 시작 시 자동 로드)

## Constraints
- 데이터에 없는 내용은 추론하지 마
- 출력 스키마의 필드 이름과 타입을 변경하지 마

## Schema 보호 규칙 (사람용 설명, 불릿 아님)
출력 스키마 필수 필드는 비어 있을 수 없으며, 린터가 중복/빈 필드를 제거한다.

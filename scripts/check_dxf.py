import sys

from ezdxf import recover


def report(stage: str, auditor) -> int:
    for entry in auditor.errors:
        print(f"{stage} error {entry.code}: {entry.message}", file=sys.stderr)
    # ezdxf flaws repaired on load or audit are in Auditor.fixes, not Auditor.errors
    for entry in auditor.fixes:
        print(f"{stage} fixed {entry.code}: {entry.message}", file=sys.stderr)
    return len(auditor.errors) + len(auditor.fixes)


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: check_dxf.py <drawing.dxf>", file=sys.stderr)
        return 2
    path = sys.argv[1]
    document, recover_auditor = recover.readfile(path, errors="strict")
    problems = report("recover", recover_auditor) + report("audit", document.audit())
    if problems:
        print(f"{path}: {problems} problems", file=sys.stderr)
        return 1
    print(f"{path}: DXF {document.dxfversion}, no errors")
    return 0


if __name__ == "__main__":
    sys.exit(main())

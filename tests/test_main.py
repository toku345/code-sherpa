import pytest

from main import main


def test_main_runs(capsys: pytest.CaptureFixture[str]) -> None:
    main()
    captured = capsys.readouterr()
    assert captured.out == "Hello from code-sherpa!\n"

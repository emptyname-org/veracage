"""Config TOML round-trip + tolerance to malformed input."""
from __future__ import annotations

from veracage import apps, config


def test_load_missing_returns_empty(tmp_xdg_config):
    cfg = config.load()
    assert cfg.is_empty()
    assert cfg.last_used_app is None


def test_save_then_load_roundtrip(tmp_xdg_config):
    original = config.Config(
        apps={
            "kate":   apps.App("kate", "Kate", "kate", ["/vault"]),
            "okular": apps.App("okular", "Okular", "okular"),
        },
        last_used_app="okular",
    )
    config.save(original)

    loaded = config.load()
    assert set(loaded.apps) == {"kate", "okular"}
    assert loaded.apps["kate"].exec == "kate"
    assert loaded.apps["okular"].args == []
    assert loaded.apps["kate"].args == ["/vault"]
    assert loaded.last_used_app == "okular"


def test_load_skips_malformed_entry(tmp_xdg_config, capsys):
    """An [apps.X] table missing a required key should be skipped, not crash.
    A leftover `category` key (from an older config) is simply ignored."""
    p = config.config_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(
        '[apps.broken]\n'
        '# missing name/exec\n'
        'args = []\n'
        '\n'
        '[apps.kate]\n'
        'name = "Kate"\n'
        'category = "text"\n'   # legacy key — must be tolerated, not required
        'exec = "kate"\n'
        'args = ["/vault"]\n'
    )
    cfg = config.load()
    assert "broken" not in cfg.apps
    assert "kate" in cfg.apps
    err = capsys.readouterr().err
    assert "broken" in err


def test_save_escapes_quotes_and_backslashes(tmp_xdg_config):
    weird = apps.App(
        key="weird",
        name='Has "quotes" and \\ slashes',
        exec="weird",
        args=['arg with "quote"'],
    )
    config.save(config.Config(apps={"weird": weird}))
    loaded = config.load()
    got = loaded.apps["weird"]
    assert got.name == 'Has "quotes" and \\ slashes'
    assert got.args == ['arg with "quote"']


# ------------------------------------------------ gpu / suspend / volumes ---

def test_default_gpu_off_and_suspend_dismount(tmp_xdg_config):
    cfg = config.load()
    assert cfg.gpu is False
    assert cfg.suspend_action == "dismount"


def test_exchange_default_on_and_roundtrips(tmp_xdg_config):
    assert config.load().exchange is True  # default on
    config.save(config.Config(
        apps={"kate": apps.App("kate", "Kate", "kate", ["/vault"])},
        exchange=False,
    ))
    assert config.load().exchange is False


def test_roundtrip_gpu_and_suspend_action(tmp_xdg_config):
    config.save(config.Config(
        apps={"kate": apps.App("kate", "Kate", "kate", ["/vault"])},
        gpu=True, suspend_action="ignore",
    ))
    loaded = config.load()
    assert loaded.gpu is True
    assert loaded.suspend_action == "ignore"


def test_invalid_suspend_action_falls_back(tmp_xdg_config, capsys):
    p = config.config_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text('[default]\nsuspend_action = "explode"\n')
    cfg = config.load()
    assert cfg.suspend_action == "dismount"
    assert "suspend_action" in capsys.readouterr().err


def test_per_volume_gpu_override(tmp_xdg_config):
    config.save(config.Config(
        apps={"okular": apps.App("okular", "Okular", "okular")},
        gpu=False,
        volumes={config._norm_vault("/tmp/work.vc"): config.VolumeConfig(gpu=True)},
    ))
    loaded = config.load()
    assert loaded.gpu_for("/tmp/work.vc") is True     # per-volume override
    assert loaded.gpu_for("/tmp/other.vc") is False   # inherits [default]


def test_per_volume_inherits_default_when_gpu_unset(tmp_xdg_config):
    config.save(config.Config(
        apps={},
        gpu=True,
        volumes={config._norm_vault("/tmp/x.vc"):
                 config.VolumeConfig(default_app="okular")},
    ))
    loaded = config.load()
    assert loaded.gpu_for("/tmp/x.vc") is True              # gpu unset -> default
    assert loaded.default_app_for("/tmp/x.vc") == "okular"
    assert loaded.default_app_for("/tmp/none.vc") is None


# ------------------------------------------------ robustness to bad input ---

def test_malformed_toml_degrades_to_empty(tmp_xdg_config, capsys):
    p = config.config_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text("this is not = valid = toml [[[\n")
    cfg = config.load()
    assert cfg.is_empty()
    assert "cannot read" in capsys.readouterr().err


def test_apps_not_a_table_is_ignored(tmp_xdg_config):
    p = config.config_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text('apps = "nope"\n')
    assert config.load().is_empty()


def test_gpu_string_does_not_fail_open(tmp_xdg_config, capsys):
    p = config.config_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text('[default]\ngpu = "false"\n')   # truthy string, not a bool
    cfg = config.load()
    assert cfg.gpu is False
    assert "gpu" in capsys.readouterr().err


def test_per_volume_gpu_string_does_not_fail_open(tmp_xdg_config):
    p = config.config_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text('[volumes."/tmp/x.vc"]\ngpu = "yes"\n')
    assert config.load().gpu_for("/tmp/x.vc") is False


def test_control_chars_roundtrip(tmp_xdg_config):
    """A name with a newline/tab must serialize to valid TOML (so the next
    load doesn't choke)."""
    weird = apps.App(key="w", name="line1\nline2\ttab", exec="w", args=[])
    config.save(config.Config(apps={"w": weird}))
    loaded = config.load()
    assert loaded.apps["w"].name == "line1\nline2\ttab"


# ----------------------------------------------------------------- backend --

def test_default_backend_is_auto(tmp_xdg_config):
    assert config.load().backend_for("/tmp/x.vc") == "auto"


def test_per_volume_backend_roundtrip(tmp_xdg_config):
    config.save(config.Config(
        apps={},
        volumes={config._norm_vault("/tmp/x.vc"): config.VolumeConfig(backend="luks")},
    ))
    loaded = config.load()
    assert loaded.backend_for("/tmp/x.vc") == "luks"
    assert loaded.backend_for("/tmp/other.vc") == "auto"


def test_invalid_backend_ignored(tmp_xdg_config, capsys):
    p = config.config_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text('[volumes."/tmp/x.vc"]\nbackend = "rot13"\n')
    cfg = config.load()
    assert cfg.backend_for("/tmp/x.vc") == "auto"
    assert "backend" in capsys.readouterr().err

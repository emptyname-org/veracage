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
            "kate":   apps.KNOWN_APPS["kate"],
            "okular": apps.KNOWN_APPS["okular"],
        },
        last_used_app="okular",
    )
    config.save(original)

    loaded = config.load()
    assert set(loaded.apps) == {"kate", "okular"}
    assert loaded.apps["kate"].category == "text"
    assert loaded.apps["okular"].args == []
    assert loaded.apps["kate"].args == ["/vault"]
    assert loaded.last_used_app == "okular"


def test_save_preserves_note(tmp_xdg_config):
    cfg = config.Config(apps={"lowriter": apps.KNOWN_APPS["lowriter"]})
    config.save(cfg)
    loaded = config.load()
    assert loaded.apps["lowriter"].note != ""


def test_load_skips_malformed_entry(tmp_xdg_config, capsys):
    """An [apps.X] table missing a required key should be skipped, not crash."""
    p = config.config_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(
        '[apps.broken]\n'
        '# missing name/category/exec\n'
        'args = []\n'
        '\n'
        '[apps.kate]\n'
        'name = "Kate"\n'
        'category = "text"\n'
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
        category="text",
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


def test_roundtrip_gpu_and_suspend_action(tmp_xdg_config):
    config.save(config.Config(
        apps={"kate": apps.KNOWN_APPS["kate"]},
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
        apps={"okular": apps.KNOWN_APPS["okular"]},
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

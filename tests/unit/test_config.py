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
            "kate":   apps.App("kate", "Kate", "kate"),
            "okular": apps.App("okular", "Okular", "okular"),
        },
        last_used_app="okular",
    )
    config.save(original)

    loaded = config.load()
    assert set(loaded.apps) == {"kate", "okular"}
    assert loaded.apps["kate"].exec == "kate"
    assert loaded.last_used_app == "okular"


def test_load_ignores_legacy_args(tmp_xdg_config):
    """Older configs carried an `args` launch-argument list; the feature was
    removed. The key is tolerated on load and dropped on save."""
    p = config.config_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(
        '[apps.kate]\n'
        'name = "Kate"\n'
        'exec = "kate"\n'
        'args = ["/vault/Docs", 123, "-b"]\n'
    )
    cfg = config.load()                      # must not raise
    assert cfg.apps["kate"].exec == "kate"
    assert not hasattr(cfg.apps["kate"], "args")
    config.save(cfg)
    assert "args" not in p.read_text()


def test_load_capitalizes_app_names(tmp_xdg_config):
    """Bare lowercase names are shown capitalized (kate -> Kate)."""
    p = config.config_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text('[apps.kate]\nname = "kate"\nexec = "kate"\n')
    assert config.load().apps["kate"].name == "Kate"


def test_ui_font_and_window_size_roundtrip(tmp_xdg_config):
    config.save(config.Config(apps={}, ui_font="dejavu-mono", ui_font_size="14",
                              window_size="1280x800"))
    loaded = config.load()
    assert loaded.ui_font == "dejavu-mono"
    assert loaded.ui_font_size == "14"
    assert loaded.window_size == "1280x800"


def test_ui_font_and_window_size_defaults(tmp_xdg_config):
    cfg = config.load()
    assert cfg.ui_font == "system"          # follow the host desktop
    assert cfg.ui_font_size == "system"
    assert cfg.window_size == "default"


def test_invalid_ui_font_and_window_size_fall_back(tmp_xdg_config, capsys):
    p = config.config_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text('[default]\nui_font = "comic"\nui_font_size = "99"\n'
                 'window_size = "9999999x1"\n')
    cfg = config.load()
    assert cfg.ui_font == "system"
    assert cfg.ui_font_size == "system"
    assert cfg.window_size == "default"
    err = capsys.readouterr().err
    assert "ui_font" in err and "window_size" in err


def test_load_dedupes_by_exec_basename(tmp_xdg_config):
    """The same binary enabled twice (bare name + absolute path) collapses to
    the first entry."""
    p = config.config_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(
        '[apps.dolphin]\n'
        'name = "dolphin"\n'
        'exec = "/usr/bin/dolphin"\n'
        '\n'
        '[apps.dolphin-2]\n'
        'name = "Dolphin"\n'
        'exec = "dolphin"\n'
        '\n'
        '[apps.kate]\n'
        'name = "Kate"\n'
        'exec = "kate"\n'
    )
    cfg = config.load()
    assert set(cfg.apps) == {"dolphin", "kate"}
    assert cfg.apps["dolphin"].exec == "/usr/bin/dolphin"


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
        'category = "text"\n'   # legacy key - must be tolerated, not required
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
    )
    config.save(config.Config(apps={"weird": weird}))
    loaded = config.load()
    got = loaded.apps["weird"]
    assert got.name == 'Has "quotes" and \\ slashes'


# ------------------------------------------------ gpu / suspend / volumes ---

def test_default_gpu_off_and_suspend_dismount(tmp_xdg_config):
    cfg = config.load()
    assert cfg.gpu is False
    assert cfg.suspend_action == "dismount"


def test_exchange_default_on_and_roundtrips(tmp_xdg_config):
    assert config.load().exchange is True  # default on
    config.save(config.Config(
        apps={"kate": apps.App("kate", "Kate", "kate")},
        exchange=False,
    ))
    assert config.load().exchange is False


def test_roundtrip_gpu_and_suspend_action(tmp_xdg_config):
    config.save(config.Config(
        apps={"kate": apps.App("kate", "Kate", "kate")},
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
    load doesn't choke). load() capitalizes the first character for display."""
    weird = apps.App(key="w", name="line1\nline2\ttab", exec="w")
    config.save(config.Config(apps={"w": weird}))
    loaded = config.load()
    assert loaded.apps["w"].name == "Line1\nline2\ttab"


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


def test_pt_to_px_default_dpi(monkeypatch):
    # No forceFontDPI -> 96 dpi -> 12pt renders at 16px (matches Qt/KDE).
    monkeypatch.setattr(config, "_font_dpi", lambda: 96.0)
    assert config._pt_to_px(12.0) == 16
    assert config._pt_to_px(10.0) == 13   # round(13.33)


def test_hinted_ascent_factor_garbage_is_neutral(tmp_path):
    # Non-font / truncated bytes must not raise and must yield the neutral 1.0.
    p = tmp_path / "x.ttf"
    p.write_bytes(b"not a font")
    assert config._hinted_ascent_factor(str(p), 16.0) == 1.0
    assert config._hinted_ascent_factor("/no/such/file", 16.0) == 1.0


def test_hinted_ascent_factor_real_font_in_clamp():
    # A real installed font yields a factor within the [1.0, 1.25] clamp.
    path = config.font_file("dejavu") or config.font_file("noto")
    if path:
        f = config._hinted_ascent_factor(path, 16.0)
        assert 1.0 <= f <= 1.25


def test_clip_clear_defaults_when_absent(tmp_xdg_config):
    # A config with no clip_clear keys must default to the secure policy.
    p = config.config_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text("[default]\ntheme = \"light\"\n")
    cfg = config.load()
    assert cfg.clip_clear is True
    assert cfg.clip_clear_timeout == config.DEFAULT_CLIP_CLEAR_TIMEOUT


def test_clip_clear_roundtrip(tmp_xdg_config):
    original = config.Config(apps={}, clip_clear=False, clip_clear_timeout=45)
    config.save(original)
    loaded = config.load()
    assert loaded.clip_clear is False
    assert loaded.clip_clear_timeout == 45


def test_clip_clear_timeout_clamped_and_typechecked(tmp_xdg_config):
    # Out of range clamps; a non-int (incl. bool) falls back to the default.
    assert config._coerce_clip_timeout(0) == config._CLIP_CLEAR_TIMEOUT_MIN
    assert config._coerce_clip_timeout(10_000_000) == config._CLIP_CLEAR_TIMEOUT_MAX
    assert config._coerce_clip_timeout(30) == 30
    assert config._coerce_clip_timeout("30") == config.DEFAULT_CLIP_CLEAR_TIMEOUT
    assert config._coerce_clip_timeout(True) == config.DEFAULT_CLIP_CLEAR_TIMEOUT


def test_clip_clear_published(tmp_xdg_config, tmp_path, monkeypatch):
    # publish_apps writes pub/clipclear as "<0|1>\n<secs>".
    pub = tmp_path / "pub"
    pub.mkdir()
    monkeypatch.setenv("VERACAGE_PUB_DIR", str(pub))
    config.publish_apps(config.Config(apps={}, clip_clear=True, clip_clear_timeout=20))
    assert (pub / "clipclear").read_text() == "1\n20\n"

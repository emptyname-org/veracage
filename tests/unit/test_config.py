"""Config TOML round-trip + tolerance to malformed input."""
from __future__ import annotations

from pathlib import Path

import pytest

from veracage import apps, config
from veracage.apps import App


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


# ---------------------------------------------------- suspend / volumes ---

def test_default_suspend_dismount(tmp_xdg_config):
    assert config.load().suspend_action == "dismount"


def test_exchange_default_on_and_roundtrips(tmp_xdg_config):
    assert config.load().exchange is True  # default on
    config.save(config.Config(
        apps={"kate": apps.App("kate", "Kate", "kate")},
        exchange=False,
    ))
    assert config.load().exchange is False


def test_roundtrip_suspend_action(tmp_xdg_config):
    config.save(config.Config(
        apps={"kate": apps.App("kate", "Kate", "kate")},
        suspend_action="ignore",
    ))
    assert config.load().suspend_action == "ignore"


def test_invalid_suspend_action_falls_back(tmp_xdg_config, capsys):
    p = config.config_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text('[default]\nsuspend_action = "explode"\n')
    cfg = config.load()
    assert cfg.suspend_action == "dismount"
    assert "suspend_action" in capsys.readouterr().err


def test_per_volume_default_app(tmp_xdg_config):
    config.save(config.Config(
        apps={},
        volumes={config._norm_vault("/tmp/x.vc"):
                 config.VolumeConfig(default_app="okular")},
    ))
    loaded = config.load()
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


def test_legacy_gpu_keys_ignored_and_dropped(tmp_xdg_config):
    # The removed GPU toggle: old configs still carry the keys. They load
    # without error or warning and disappear on the next save.
    p = config.config_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text('[default]\ngpu = true\n[volumes."/tmp/x.vc"]\ngpu = true\n')
    cfg = config.load()
    config.save(cfg)
    assert "gpu" not in p.read_text()


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


def test_auto_dismount_roundtrip_and_bounds(tmp_xdg_config, tmp_path, monkeypatch):
    config.save(config.Config(apps={}, auto_dismount=120))
    assert config.load().auto_dismount == 120
    # Off by default, bounded above, and a non-int falls back to off rather than
    # leaving a volume open forever.
    assert config.Config(apps={}).auto_dismount == 0
    assert config._coerce_auto_dismount(10_000) == config._AUTO_DISMOUNT_MAX
    assert config._coerce_auto_dismount("30") == 0
    assert config._coerce_auto_dismount(True) == 0
    pub = tmp_path / "pub"
    pub.mkdir()
    monkeypatch.setenv("VERACAGE_PUB_DIR", str(pub))
    config.publish_apps(config.Config(apps={}, auto_dismount=30))
    assert (pub / "autodismount").read_text() == "30\n"


def test_modifier_keys_roundtrip_and_validation(tmp_xdg_config):
    config.save(config.Config(apps={}, modifier_keys="altwin:ctrl_win"))
    assert config.load().modifier_keys == "altwin:ctrl_win"
    # An unknown value is refused: it reaches libxkbcommon through the
    # published keyboard file.
    p = config.config_path()
    p.write_text('[default]\nmodifier_keys = "ctrl:whatever"\n')
    assert config.load().modifier_keys == "system"


def test_modifier_options_replaces_only_the_modifier_groups():
    host = "compose:caps,eurosign:e,altwin:ctrl_win"
    assert config.modifier_options(host, "system") == host
    # The Compose key and the currency sign survive a modifier choice.
    assert config.modifier_options(host, "ctrl:nocaps") == "compose:caps,eurosign:e,ctrl:nocaps"
    assert config.modifier_options(host, "none") == "compose:caps,eurosign:e"
    assert config.modifier_options("", "altwin:alt_win") == "altwin:alt_win"


def test_keyboard_published(tmp_xdg_config, tmp_path, monkeypatch):
    # publish_apps writes pub/keyboard as "<model>\n<layout>\n<variant>\n<options>".
    pub = tmp_path / "pub"
    pub.mkdir()
    monkeypatch.setenv("VERACAGE_PUB_DIR", str(pub))
    monkeypatch.setattr(config, "host_keyboard",
                        lambda: ("apple", "us,rp", ",", "compose:caps,altwin:ctrl_win"))
    config.publish_apps(config.Config(apps={}, modifier_keys="ctrl:nocaps"))
    assert (pub / "keyboard").read_text() == "apple\nus,rp\n,\ncompose:caps,ctrl:nocaps\n"


@pytest.fixture
def tmp_xdg_data(monkeypatch, tmp_path):
    """An isolated XDG data dir with an applications/ directory, so mimeapps
    tests never scan the real host .desktop files."""
    data = tmp_path / "data"
    (data / "applications").mkdir(parents=True)
    monkeypatch.setenv("XDG_DATA_HOME", str(data))
    monkeypatch.setenv("XDG_DATA_DIRS", str(tmp_path / "no-such-data"))
    return data / "applications"


def _desktop_file(apps_dir, name, exec_line, mimes):
    (apps_dir / name).write_text(
        f"[Desktop Entry]\nName=X\nExec={exec_line}\nMimeType={mimes}\n")


def test_mimeapps_enabled_apps_win_over_host_defaults(
        tmp_xdg_config, tmp_xdg_data):
    _desktop_file(tmp_xdg_data, "org.kde.kate.desktop", "kate %U",
                  "text/plain;text/markdown;")
    _desktop_file(tmp_xdg_data, "okularApplication_pdf.desktop", "okular %U",
                  "application/x-okular;")
    (tmp_xdg_config / "mimeapps.list").write_text(
        "[Default Applications]\n"
        "text/plain=kwrite.desktop;\n"
        "application/pdf=okularApplication_pdf.desktop;\n"
        "[Added Associations]\n"
        "image/png=gwenview.desktop;\n")
    cfg = config.Config(apps={
        "kate": apps.App("kate", "Kate", "kate"),
        "okular": apps.App("okular", "Okular", "okular"),
    })
    lines = config._mimeapps_body(cfg).splitlines()
    # The enabled app claims its declared types, beating the host default.
    assert "text/plain=org.kde.kate.desktop;" in lines
    assert "text/markdown=org.kde.kate.desktop;" in lines
    # A host association for a type no enabled app declares is kept, because
    # its handler IS an enabled app.
    assert "application/pdf=okularApplication_pdf.desktop;" in lines


def test_mimeapps_drops_host_handlers_that_are_not_enabled(
        tmp_xdg_config, tmp_xdg_data):
    """Only enabled apps may be named. An unrestricted host entry would let a
    click in the sandbox start an app the user never enabled (the classic case:
    x-scheme-handler/http=firefox.desktop, launched with no network)."""
    _desktop_file(tmp_xdg_data, "org.kde.kate.desktop", "kate %U", "text/plain;")
    (tmp_xdg_config / "mimeapps.list").write_text(
        "[Default Applications]\n"
        "x-scheme-handler/http=firefox-esr.desktop;\n"
        "x-scheme-handler/mailto=thunderbird.desktop;\n"
        "application/pdf=okular.desktop;firefox-esr.desktop;\n"
        "[Added Associations]\n"
        "image/png=gwenview.desktop;\n")
    cfg = config.Config(apps={"kate": apps.App("kate", "Kate", "kate")})
    body = config._mimeapps_body(cfg)
    for unwanted in ("firefox", "thunderbird", "okular", "gwenview",
                     "x-scheme-handler"):
        assert unwanted not in body
    assert body.splitlines() == ["[Default Applications]",
                                 "text/plain=org.kde.kate.desktop;"]


def test_mimeapps_first_enabled_app_wins_on_overlap(
        tmp_xdg_config, tmp_xdg_data):
    _desktop_file(tmp_xdg_data, "a.desktop", "aedit", "text/plain;")
    _desktop_file(tmp_xdg_data, "b.desktop", "bedit", "text/plain;")
    cfg = config.Config(apps={
        "bedit": apps.App("bedit", "Bedit", "bedit"),
        "aedit": apps.App("aedit", "Aedit", "aedit"),
    })
    assert "text/plain=b.desktop;" in config._mimeapps_body(cfg).splitlines()


def test_mimeapps_published_and_empty_without_desktop_files(
        tmp_xdg_config, tmp_xdg_data, tmp_path, monkeypatch):
    pub = tmp_path / "pub"
    pub.mkdir()
    monkeypatch.setenv("VERACAGE_PUB_DIR", str(pub))
    config.publish_apps(config.Config(
        apps={"kate": apps.App("kate", "Kate", "kate")}))
    # No .desktop files and no host mimeapps.list: just the empty section.
    assert (pub / "mimeapps.list").read_text() == "[Default Applications]\n"


def test_an_app_key_that_needs_quoting_survives_a_save(tmp_xdg_config):
    """A key with a space is legal TOML when quoted, and both the hand-edited
    form and `configure --key "my key"` produce one. Written back bare it made
    the file unparsable, and the next load silently returned an EMPTY config:
    every app, the theme, the shortcuts and the per-volume overrides gone."""
    cfg = config.load()
    cfg.theme = "dark"
    cfg.apps = {"my key": App(key="my key", name="X", exec="true")}
    config.save(cfg)

    again = config.load()
    assert list(again.apps) == ["my key"]
    assert again.apps["my key"].exec == "true"
    assert again.theme == "dark"


def test_a_non_string_exchange_dir_does_not_crash_the_open_path(tmp_xdg_config):
    """`cfg.exchange_path()` is called on the `veracage open` path, outside the
    try that guards the shared directory, so a wrongly-typed value used to end
    the command in a TypeError traceback."""
    (tmp_xdg_config / "veracage").mkdir(parents=True, exist_ok=True)
    (tmp_xdg_config / "veracage" / "config.toml").write_text(
        "[default]\nexchange_dir = 123\n")
    cfg = config.load()
    assert isinstance(cfg.exchange_path(), Path)


# ------------------------------------------------------------- shortcuts ---

def test_shortcuts_round_trip_and_a_bad_bind_falls_back(tmp_xdg_config, capsys):
    """The compositor matches these against every key press and acts on them by
    moving the clipboard across the sandbox boundary, so a malformed bind must
    not reach it: it falls back to the default instead."""
    cfg = config.load()
    cfg.shortcuts = {"copy_out": "Ctrl+Shift+C", "paste_in": "Ctrl+Shift+V"}
    config.save(cfg)
    assert config.load().shortcuts == {"copy_out": "Ctrl+Shift+C",
                                       "paste_in": "Ctrl+Shift+V"}

    (tmp_xdg_config / "veracage").mkdir(parents=True, exist_ok=True)
    (tmp_xdg_config / "veracage" / "config.toml").write_text(
        '[shortcuts]\ncopy_out = ""\npaste_in = "Ctrl+Alt+V"\n')
    loaded = config.load()
    assert loaded.shortcuts["copy_out"] == "Ctrl+Alt+C", "an empty bind must not stick"
    assert loaded.shortcuts["paste_in"] == "Ctrl+Alt+V"
    assert "invalid shortcut" in capsys.readouterr().err


def test_an_unknown_theme_falls_back_to_light(tmp_xdg_config):
    """theme is published to the compositor and seeded into every sandbox's
    kdeglobals; an arbitrary string there is not something to pass on."""
    (tmp_xdg_config / "veracage").mkdir(parents=True, exist_ok=True)
    (tmp_xdg_config / "veracage" / "config.toml").write_text(
        '[default]\ntheme = "neon"\n')
    assert config.load().theme == "light"


def test_a_string_is_not_a_boolean(tmp_xdg_config):
    """TOML `exchange = "false"` is a STRING, and a truthy one. Treating it as
    true would mount the shared directory for someone who wrote it to turn the
    thing off."""
    (tmp_xdg_config / "veracage").mkdir(parents=True, exist_ok=True)
    (tmp_xdg_config / "veracage" / "config.toml").write_text(
        '[default]\nexchange = "false"\nclip_clear = "yes"\n')
    cfg = config.load()
    assert cfg.exchange is False
    assert cfg.clip_clear is False


def test_the_config_is_replaced_atomically(tmp_xdg_config, monkeypatch):
    """A concurrent `veracage open` reads this file while a dialog saves it. A
    truncate-then-write leaves a window where it parses as EMPTY, and an empty
    config then makes auto-detect overwrite the user's curated app list."""
    cfg = config.load()
    cfg.apps = {"kate": App(key="kate", name="Kate", exec="kate")}
    config.save(cfg)
    before = config.config_path().read_text()

    # A save that dies mid-write must leave the previous file untouched.
    def boom(self, target):
        raise OSError("crash between write and rename")
    monkeypatch.setattr(config.Path, "replace", boom)
    cfg.apps = {"okular": App(key="okular", name="Okular", exec="okular")}
    with pytest.raises(OSError):
        config.save(cfg)
    assert config.config_path().read_text() == before
    assert list(config.load().apps) == ["kate"]


def test_publish_apps_drops_a_key_that_would_break_the_menu_file(tmp_path, monkeypatch):
    """`config.apps` is `<key>\t<name>` per line and the key also names an icon
    file, so a key with a slash or a traversal must never be published."""
    monkeypatch.setenv("VERACAGE_PUB_DIR", str(tmp_path))
    cfg = config.Config(apps={
        "kate": App(key="kate", name="Kate", exec="kate"),
        "../evil": App(key="../evil", name="Evil", exec="evil"),
        "a/b": App(key="a/b", name="Slash", exec="slash"),
    })
    config.publish_apps(cfg)
    body = (tmp_path / "config.apps").read_text()
    assert "kate\tKate" in body
    assert "evil" not in body and "a/b" not in body


def test_log_dir_roundtrip_and_validation(tmp_xdg_config, capsys):
    config.save(config.Config(apps={}, log_dir="/var/log/veracage"))
    assert config.load().log_dir == "/var/log/veracage"
    # Unset by default, and only an absolute path is taken: everything else
    # means "the runtime dir", loudly for the values that look like a mistake.
    assert config.Config(apps={}).log_dir is None
    assert config._coerce_log_dir(None) is None
    assert config._coerce_log_dir("") is None
    assert config._coerce_log_dir("logs") is None
    assert config._coerce_log_dir(7) is None
    err = capsys.readouterr().err
    assert err.count("invalid log_dir") == 2

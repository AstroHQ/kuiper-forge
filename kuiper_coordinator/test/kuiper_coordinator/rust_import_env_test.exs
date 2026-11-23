defmodule KuiperCoordinator.RustImportEnvTest do
  # Expected values come from figment 0.10.19 (the Rust coordinator's Cargo.lock version) loading
  # `Env::prefixed("KUIPER_").split("__")`, the way kuiper-forge's config.rs does.
  use ExUnit.Case, async: true

  alias KuiperCoordinator.RustImport.Config

  @base %{
    "KUIPER_GITHUB__APP_ID" => "12345",
    "KUIPER_GITHUB__PRIVATE_KEY" =>
      "-----BEGIN RSA PRIVATE KEY-----\\nabc\\n-----END RSA PRIVATE KEY-----",
    "KUIPER_PROVISIONING__MODE" => "webhook",
    "KUIPER_PROVISIONING__WEBHOOK__SECRET" => "s3cret"
  }

  @values [
    {"s3cret\n", "s3cret"},
    {" padded ", "padded"},
    {"line1\nline2\n", "line1\nline2"},
    {"abc def", "abc def"},
    {"a#b=c", "a#b=c"},
    {"0123", 123},
    {"+5", 5},
    {"-5", -5},
    {"1.5", 1.5},
    {".5", 0.5},
    {"1.", 1.0},
    {"1.5e3", 1500.0},
    {"1e3", "1e3"},
    {"18446744073709551615", 18_446_744_073_709_551_615},
    {"18446744073709551616", "18446744073709551616"},
    {"-9223372036854775809", "-9223372036854775809"},
    {"true", true},
    {" true ", true},
    {"True", "True"},
    {"trueish", "trueish"},
    {"\"quoted\"", "quoted"},
    {"\"a\\nb\"", "a\nb"},
    {"\"bad\\q\"", "\"bad\\q\""},
    {"'x'", "x"},
    {"'xy'", "'xy'"},
    {"[self-hosted, macos]", ["self-hosted", "macos"]},
    {"[\"a\", \"b\"]", ["a", "b"]},
    {"[]", []},
    {"[a,]", ["a"]},
    {"[abc", "[abc"},
    {"a]b", "a]b"},
    {"a,b", "a,b"},
    {"{a=1, b = hi}", %{"a" => 1, "b" => "hi"}},
    {"[{name=knope, type=forgejo, url=https://git.example, token=t0k}]",
     [%{"name" => "knope", "type" => "forgejo", "url" => "https://git.example", "token" => "t0k"}]},
    {"\"unterminated", "\"unterminated"}
  ]

  test "env values parse as figment parses them" do
    for {raw, want} <- @values do
      assert Config.env_overrides(%{"KUIPER_X" => raw})["x"] == want, "KUIPER_X=#{inspect(raw)}"
    end
  end

  test "env keys: KUIPER_ prefix in any case, __ nests, empty segments skipped" do
    env = %{
      "kuiper_a__b" => "1",
      "Kuiper_C" => "2",
      "KUIPER_D___E" => "3",
      "KUIPER_I__" => "6",
      "OTHER" => "7"
    }

    assert Config.env_overrides(env) == %{"a" => %{"b" => 1}, "c" => 2, "d" => %{"_e" => 3}}
  end

  test "a webhook secret from a k8s Secret with a trailing newline imports trimmed" do
    env = Map.put(@base, "KUIPER_PROVISIONING__WEBHOOK__SECRET", "s3cret\n")
    assert {:ok, %{forges: [forge]}} = Config.load(nil, env)
    assert forge[:webhook_secret] == "s3cret"
  end

  test "unquoted required_labels import as a list" do
    env = Map.put(@base, "KUIPER_PROVISIONING__WEBHOOK__REQUIRED_LABELS", "[self-hosted, macos]")
    assert {:ok, config} = Config.load(nil, env)
    assert config.settings.required_labels == ["self-hosted", "macos"]
  end
end

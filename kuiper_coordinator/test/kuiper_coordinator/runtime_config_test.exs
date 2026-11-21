defmodule KuiperCoordinator.RuntimeConfigTest do
  # Mutates OS env vars read by config/runtime.exs.
  use ExUnit.Case, async: false

  @runtime Path.expand("../../config/runtime.exs", __DIR__)
  @vars ~w(KUIPER_HTTP_PORT PORT)

  setup do
    saved = Map.new(@vars, &{&1, System.get_env(&1)})
    Enum.each(@vars, &System.delete_env/1)

    on_exit(fn ->
      Enum.each(saved, fn
        {k, nil} -> System.delete_env(k)
        {k, v} -> System.put_env(k, v)
      end)
    end)
  end

  defp http_port do
    Config.Reader.read!(@runtime, env: :dev, target: :host)
    |> get_in([:kuiper_coordinator, KuiperCoordinatorWeb.Endpoint, :http, :port])
  end

  test "defaults to its own port, not Phoenix's 4000 or the Rust coordinator's 9443" do
    assert http_port() == 9480
  end

  test "KUIPER_HTTP_PORT sets the port" do
    System.put_env("KUIPER_HTTP_PORT", "9555")
    assert http_port() == 9555
  end

  test "PORT works as a fallback, KUIPER_HTTP_PORT wins over it" do
    System.put_env("PORT", "9600")
    assert http_port() == 9600
    System.put_env("KUIPER_HTTP_PORT", "9601")
    assert http_port() == 9601
  end
end

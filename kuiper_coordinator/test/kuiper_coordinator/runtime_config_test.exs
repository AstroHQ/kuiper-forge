defmodule KuiperCoordinator.RuntimeConfigTest do
  # Mutates OS env vars read by config/runtime.exs.
  use ExUnit.Case, async: false

  @runtime Path.expand("../../config/runtime.exs", __DIR__)
  @vars ~w(KUIPER_HTTP_PORT PORT KUIPER_AGENT_GRPC KUIPER_AGENT_PORT)

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

  defp config, do: Config.Reader.read!(@runtime, env: :dev, target: :host)
  defp http, do: get_in(config(), [:kuiper_coordinator, KuiperCoordinatorWeb.Endpoint, :http])
  defp agent_grpc, do: get_in(config(), [:kuiper_coordinator, :agent_grpc])

  test "HTTP defaults to its own port, not Phoenix's 4000 or the Rust coordinator's 9443" do
    assert http()[:port] == 9480
  end

  test "KUIPER_HTTP_PORT sets the port" do
    System.put_env("KUIPER_HTTP_PORT", "9555")
    assert http()[:port] == 9555
  end

  test "PORT works as a fallback, KUIPER_HTTP_PORT wins over it" do
    System.put_env("PORT", "9600")
    assert http()[:port] == 9600
    System.put_env("KUIPER_HTTP_PORT", "9601")
    assert http()[:port] == 9601
  end

  test "agent gRPC gets its own port, KUIPER_AGENT_PORT, default 9543, and HTTP keeps PORT (D21)" do
    System.put_env("KUIPER_AGENT_GRPC", "true")
    System.put_env("PORT", "9700")
    assert http()[:port] == 9700
    assert agent_grpc()[:enabled]
    assert agent_grpc()[:port] == 9543
    System.put_env("KUIPER_AGENT_PORT", "9701")
    assert agent_grpc()[:port] == 9701
    assert http()[:port] == 9700
  end
end

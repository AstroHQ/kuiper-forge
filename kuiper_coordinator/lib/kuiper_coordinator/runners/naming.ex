defmodule KuiperCoordinator.Runners.Naming do
  @moduledoc """
  Runner/VM names: `<short-host>-<8 hex>`, so you can tell where a runner lives from the
  forge's runner list (parity F1, Rust `runner_name_for_host`, #34).
  """

  @max_host_len 20

  @doc "A new unique name for a runner on the agent with this hostname."
  @spec new(String.t() | nil) :: String.t()
  def new(hostname) do
    suffix = :crypto.strong_rand_bytes(4) |> Base.encode16(case: :lower)
    for_host(hostname, suffix)
  end

  @doc "`<short-host>-<suffix>`, `runner-<suffix>` when there's no usable hostname."
  @spec for_host(String.t() | nil, String.t()) :: String.t()
  def for_host(hostname, suffix) do
    host =
      (hostname || "")
      |> String.split(".", parts: 2)
      |> hd()
      |> String.to_charlist()
      |> Enum.map(fn c ->
        cond do
          c in ?a..?z or c in ?0..?9 -> c
          c in ?A..?Z -> c + 32
          true -> ?-
        end
      end)
      |> Enum.take(@max_host_len)
      |> List.to_string()
      |> String.trim("-")

    host = if host == "", do: "runner", else: host
    "#{host}-#{suffix}"
  end
end

defmodule KuiperCoordinator.Agents.Bundle do
  @moduledoc """
  Registration bundle string, the same format as `kuiper-agent-lib/src/bundle.rs`:
  `kfr1_` + URL-safe base64 (no padding) of JSON `{"t": token, "ca": pem, "m": "ca" | "chain", "u": url}`.
  `ca` is left out when there is no CA PEM (chain mode with public roots).
  """

  @prefix "kfr1_"

  @spec encode(String.t(), String.t() | nil, String.t(), String.t()) :: String.t()
  def encode(token, ca_pem, trust_mode, coordinator_url) when trust_mode in ["ca", "chain"] do
    json =
      [{"t", token}]
      |> then(fn kv -> if ca_pem in [nil, ""], do: kv, else: kv ++ [{"ca", ca_pem}] end)
      |> Kernel.++([{"m", trust_mode}, {"u", coordinator_url}])
      |> Jason.OrderedObject.new()
      |> Jason.encode!()

    @prefix <> Base.url_encode64(json, padding: false)
  end

  @spec decode(String.t()) :: {:ok, map()} | {:error, atom()}
  def decode(@prefix <> payload) do
    with {:ok, json} <- Base.url_decode64(payload, padding: false),
         {:ok, map} <- Jason.decode(json) do
      {:ok, map}
    else
      _ -> {:error, :invalid_bundle}
    end
  end

  def decode(_), do: {:error, :invalid_prefix}
end

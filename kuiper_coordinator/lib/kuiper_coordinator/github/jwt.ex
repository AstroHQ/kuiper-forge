defmodule KuiperCoordinator.GitHub.JWT do
  @moduledoc """
  GitHub App JWT (parity G1): RS256, `iat` 60 s in the past for clock drift, `exp` 9 minutes out
  (GitHub's max is 10), `iss` the App id.
  """

  @spec app_jwt(integer() | String.t(), String.t(), integer()) ::
          {:ok, String.t()} | {:error, :invalid_private_key}
  def app_jwt(app_id, pem, now_unix) do
    with {:ok, key} <- decode_key(pem) do
      header = encode(%{"alg" => "RS256", "typ" => "JWT"})

      claims =
        encode(%{"iat" => now_unix - 60, "exp" => now_unix + 9 * 60, "iss" => to_string(app_id)})

      signing_input = header <> "." <> claims
      signature = :public_key.sign(signing_input, :sha256, key)
      {:ok, signing_input <> "." <> Base.url_encode64(signature, padding: false)}
    end
  end

  @doc "Parses an RSA private key PEM; literal `\\n` (from an env var) count as newlines."
  @spec decode_key(String.t()) :: {:ok, tuple()} | {:error, :invalid_private_key}
  def decode_key(pem) do
    pem = String.replace(pem, "\\n", "\n")

    case :public_key.pem_decode(pem) do
      [entry | _] ->
        case :public_key.pem_entry_decode(entry) do
          {:RSAPrivateKey, _, _, _, _, _, _, _, _, _, _} = key -> {:ok, key}
          _ -> {:error, :invalid_private_key}
        end

      [] ->
        {:error, :invalid_private_key}
    end
  rescue
    _ -> {:error, :invalid_private_key}
  end

  defp encode(map), do: map |> Jason.encode!() |> Base.url_encode64(padding: false)
end

defmodule KuiperCoordinator.Admin.ApiTokens do
  @moduledoc """
  Tokens for the read-only `/api/v1` JSON API (parity UI9, api_tokens.rs): `kfapi_` plus 40
  alphanumerics, shown once at creation. Only the SHA-256 is stored (they're long random
  strings, so a plain hash is enough and keeps per-request checks cheap) plus the first 6
  characters after the prefix, so tokens can be told apart in the UI.
  """
  import Ecto.Query

  alias KuiperCoordinator.Admin.ApiToken
  alias KuiperCoordinator.Repo

  @prefix "kfapi_"
  @secret_len 40
  @display_chars 6
  @alnum ~c"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"

  def prefix, do: @prefix

  @doc "Creates a token named `name` (1-100 characters, trimmed). Returns the plaintext once."
  @spec create(String.t(), String.t()) ::
          {:ok, String.t(), ApiToken.t()} | {:error, :invalid_name | Ecto.Changeset.t()}
  def create(name, created_by) do
    name = String.trim(name || "")

    if name == "" or String.length(name) > 100 do
      {:error, :invalid_name}
    else
      token = @prefix <> random_alnum(@secret_len)

      %ApiToken{
        name: name,
        token_hash: hash(token),
        token_prefix: binary_part(token, 0, byte_size(@prefix) + @display_chars),
        created_by: created_by
      }
      |> Repo.insert()
      |> case do
        {:ok, record} -> {:ok, token, record}
        error -> error
      end
    end
  end

  @doc "All tokens, newest first."
  def list, do: Repo.all(from(t in ApiToken, order_by: [desc: t.inserted_at, desc: t.id]))

  def delete(id) when is_binary(id) do
    with {:ok, uuid} <- Ecto.UUID.cast(id),
         {1, _} <- Repo.delete_all(from(t in ApiToken, where: t.id == ^uuid)) do
      :ok
    else
      _ -> {:error, :not_found}
    end
  end

  @doc "The record for a presented token, bumping its last-used time (best effort); nil if invalid."
  @spec validate(String.t() | nil) :: ApiToken.t() | nil
  def validate(@prefix <> _ = token) do
    case Repo.get_by(ApiToken, token_hash: hash(token)) do
      nil ->
        nil

      record ->
        now = DateTime.utc_now()

        try do
          Repo.update_all(from(t in ApiToken, where: t.id == ^record.id),
            set: [last_used_at: now]
          )
        rescue
          # a failed bump shouldn't fail the request
          _ -> :ok
        end

        %{record | last_used_at: now}
    end
  end

  def validate(_), do: nil

  defp hash(token), do: :crypto.hash(:sha256, token)

  # rejection sampling over 62 symbols, so every character is uniform
  defp random_alnum(n), do: random_alnum(n, [])
  defp random_alnum(0, acc), do: List.to_string(acc)

  defp random_alnum(n, acc) do
    <<b>> = :crypto.strong_rand_bytes(1)

    if b < 248 do
      random_alnum(n - 1, [Enum.at(@alnum, rem(b, 62)) | acc])
    else
      random_alnum(n, acc)
    end
  end
end

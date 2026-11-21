defmodule KuiperCoordinator.Forge.RunnerScope.Type do
  @moduledoc "Stores a runner scope as its config string (`org`, `owner/repo`, `@user`, `*`)."
  use Ecto.Type
  alias KuiperCoordinator.Forge.RunnerScope

  def type, do: :string

  def cast(s) when is_binary(s), do: s |> RunnerScope.parse() |> ok_or_error()
  def cast(scope), do: if(valid?(scope), do: {:ok, scope}, else: :error)

  def load(s) when is_binary(s), do: s |> RunnerScope.parse() |> ok_or_error()
  def dump(scope), do: if(valid?(scope), do: {:ok, RunnerScope.to_string(scope)}, else: :error)

  defp valid?({:org, o}) when is_binary(o), do: true
  defp valid?({:repo, o, r}) when is_binary(o) and is_binary(r), do: true
  defp valid?({:user, u}) when is_binary(u), do: true
  defp valid?(:instance), do: true
  defp valid?(_), do: false

  defp ok_or_error({:ok, v}), do: {:ok, v}
  defp ok_or_error(_), do: :error
end

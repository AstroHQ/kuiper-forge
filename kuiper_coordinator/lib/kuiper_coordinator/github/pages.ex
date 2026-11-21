defmodule KuiperCoordinator.GitHub.Pages do
  @moduledoc """
  Collects every page of a GitHub list endpoint (parity G5, #37). Stops after the first short
  page; a failed page fails the whole listing, so a partial list is never mistaken for the
  complete one; capped at #{100} pages.
  """
  require Logger

  @page_size 100
  @max_pages 100

  def page_size, do: @page_size

  @spec collect((pos_integer() -> {:ok, list()} | {:error, term()})) ::
          {:ok, list()} | {:error, {:page, pos_integer(), term()}}
  def collect(fetch_page), do: collect(fetch_page, 1, [])

  defp collect(_fetch, page, acc) when page > @max_pages do
    items = acc |> Enum.reverse() |> Enum.concat()

    Logger.warning(
      "Stopped after #{@max_pages} pages (#{length(items)} items) - listing may be incomplete"
    )

    {:ok, items}
  end

  defp collect(fetch, page, acc) do
    case fetch.(page) do
      {:ok, items} when length(items) < @page_size ->
        {:ok, [items | acc] |> Enum.reverse() |> Enum.concat()}

      {:ok, items} ->
        collect(fetch, page + 1, [items | acc])

      {:error, reason} ->
        {:error, {:page, page, reason}}
    end
  end
end

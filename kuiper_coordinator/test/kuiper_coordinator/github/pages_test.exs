defmodule KuiperCoordinator.GitHub.PagesTest do
  # Ported from kuiper-forge/src/github.rs collect_pages_* (#37, 32b590d, parity G5)
  use ExUnit.Case, async: true

  alias KuiperCoordinator.GitHub.Pages

  defp fake_pages(sizes, test_pid) do
    fn page ->
      send(test_pid, {:page, page})
      start = sizes |> Enum.take(page - 1) |> Enum.sum()
      size = Enum.at(sizes, page - 1, 0)
      {:ok, Enum.to_list(start..(start + size - 1)//1)}
    end
  end

  defp requested do
    Stream.repeatedly(fn ->
      receive do
        {:page, n} -> n
      after
        0 -> nil
      end
    end)
    |> Enum.take_while(& &1)
  end

  test "follows full pages" do
    {:ok, items} = Pages.collect(fake_pages([100, 100, 30], self()))
    assert length(items) == 230
    assert List.last(items) == 229
    assert requested() == [1, 2, 3]
  end

  test "stops on a short or empty page" do
    assert {:ok, items} = Pages.collect(fake_pages([7], self()))
    assert length(items) == 7
    assert requested() == [1]

    # exactly one full page needs a second, empty, request to know it's done
    assert {:ok, items} = Pages.collect(fake_pages([100], self()))
    assert length(items) == 100
    assert requested() == [1, 2]
  end

  test "fails if any page fails" do
    result =
      Pages.collect(fn
        2 -> {:error, "HTTP 502"}
        _ -> {:ok, Enum.to_list(1..100)}
      end)

    assert {:error, {:page, 2, "HTTP 502"}} = result
  end

  test "is capped at 100 pages" do
    counter = :counters.new(1, [])

    {:ok, items} =
      Pages.collect(fn _ ->
        :counters.add(counter, 1, 1)
        {:ok, Enum.to_list(1..100)}
      end)

    assert :counters.get(counter, 1) == 100
    assert length(items) == 100 * 100
  end
end

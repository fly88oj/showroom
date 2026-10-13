import { describe, expect, it } from "vitest";
import { parseCsv, splitSql } from "@/lib/sql";

describe("splitSql（引号/$$ 状态机）", () => {
  it("普通多语句", () => {
    expect(splitSql("SELECT 1; UPDATE t SET x=1;")).toEqual(["SELECT 1", "UPDATE t SET x=1"]);
  });
  it("字符串字面量内分号不拆", () => {
    expect(splitSql("SELECT 'a;b' AS x; SELECT 2")).toEqual(["SELECT 'a;b' AS x", "SELECT 2"]);
  });
  it("SQL '' 转义不闭合引号", () => {
    expect(splitSql("SELECT 'it''s;ok'; SELECT 2")).toEqual(["SELECT 'it''s;ok'", "SELECT 2"]);
  });
  it("双引号标识符内分号不拆", () => {
    expect(splitSql('SELECT "a;b" FROM t; SELECT 2')).toEqual(['SELECT "a;b" FROM t', "SELECT 2"]);
  });
  it("$$ 函数体内分号不拆", () => {
    expect(splitSql("CREATE FUNCTION f() AS $$ BEGIN X; END $$; SELECT 2")).toEqual([
      "CREATE FUNCTION f() AS $$ BEGIN X; END $$", "SELECT 2",
    ]);
  });
  it("尾分号/空白与空输入", () => {
    expect(splitSql("  ; ; SELECT 1 ; ")).toEqual(["SELECT 1"]);
    expect(splitSql("")).toEqual([]);
  });
});

describe("parseCsv（psql --csv）", () => {
  it("基本表", () => {
    const { cols, rows } = parseCsv("a,b\n1,2\n3,4");
    expect(cols).toEqual(["a", "b"]);
    expect(rows).toEqual([["1", "2"], ["3", "4"]]);
  });
  it("引号内逗号/换行/双引号转义", () => {
    const { rows } = parseCsv('a\n"x,y"\n"line1\nline2"\n"say ""hi"""');
    expect(rows).toEqual([["x,y"], ["line1\nline2"], ['say "hi"']]);
  });
  it("全空行过滤", () => {
    const { rows } = parseCsv("a\n\n1\n");
    expect(rows).toEqual([["1"]]);
  });
  it("只有表头", () => {
    const { cols, rows } = parseCsv("a,b");
    expect(cols).toEqual(["a", "b"]);
    expect(rows).toEqual([]);
  });
});

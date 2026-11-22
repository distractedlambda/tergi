//! Checks tergi's tokens for Zig files against the tokenizer in Zig's
//! standard library. Usage: compare <work>
//!
//! It reads <work>/files.txt and the tokens that tergi wrote to
//! <work>/tokens, works out which color each byte should get from Zig's
//! tokens, and compares that with tergi's color for the byte. It also
//! checks that each identifier starts and ends in the same place, since
//! identifiers get their colors from their names.

const std = @import("std");

const Kind = enum {
    Comment,
    String,
    Escape,
    Number,
    Constant,
    Keyword,
    Type,
    Attribute,
    Lifetime,
    Operator,
    Punctuation,
    Identifier,
};

const Range = struct { start: usize, end: usize };

// Only the first few disagreements are printed, so that one bug doesn't
// bury the others.
const max_shown = 40;

var out_buf: [1 << 16]u8 = undefined;

fn expected(tag: std.zig.Token.Tag, text: []const u8) Kind {
    const name = @tagName(tag);
    if (std.mem.startsWith(u8, name, "keyword_")) return .Keyword;
    return switch (tag) {
        .identifier => if (isConstant(text)) .Constant else .Identifier,
        // Nobody can define new builtins, so they look like keywords.
        .builtin => .Keyword,
        // tergi may also color escapes inside these, which the comparison
        // allows.
        .string_literal, .char_literal, .multiline_string_literal_line => .String,
        .number_literal => .Number,
        .doc_comment, .container_doc_comment => .Comment,
        .l_paren, .r_paren, .l_bracket, .r_bracket, .l_brace, .r_brace => .Punctuation,
        .comma, .semicolon, .colon, .period => .Punctuation,
        else => .Operator,
    };
}

fn isConstant(text: []const u8) bool {
    for ([_][]const u8{ "true", "false", "null", "undefined" }) |c| {
        if (std.mem.eql(u8, text, c)) return true;
    }
    return false;
}

fn kindName(k: ?Kind) []const u8 {
    return if (k) |kind| @tagName(kind) else "nothing";
}

// The tokenizer skips whitespace and plain comments, so the bytes between
// its tokens are those. Anything else there would be a surprise, so it
// gets a color that tergi never gives to such bytes and shows up as a
// disagreement.
fn fillGap(src: []const u8, want: []?Kind, from: usize, to: usize) void {
    var i = from;
    while (i < to) {
        if (std.mem.startsWith(u8, src[i..], "//")) {
            while (i < to and src[i] != '\n') : (i += 1) want[i] = .Comment;
        } else {
            const bom = i < 3 and std.mem.startsWith(u8, src, "\xEF\xBB\xBF");
            if (std.mem.indexOfScalar(u8, " \t\r\n", src[i]) == null and !bom) {
                want[i] = .Operator;
            }
            i += 1;
        }
    }
}

pub fn main(init: std.process.Init) !void {
    const io = init.io;
    const gpa = init.gpa;
    const arena = init.arena.allocator();
    const args = try init.minimal.args.toSlice(arena);
    if (args.len != 2) {
        std.debug.print("usage: compare <work>\n", .{});
        std.process.exit(2);
    }
    const work = args[1];
    const cwd = std.Io.Dir.cwd();
    var w = std.Io.File.stdout().writerStreaming(io, &out_buf);
    const out = &w.interface;

    const list_path = try std.fs.path.join(arena, &.{ work, "files.txt" });
    const list = try cwd.readFileAlloc(io, list_path, arena, .unlimited);

    var files: usize = 0;
    var bytes: usize = 0;
    var skipped: usize = 0;
    var bad: usize = 0;
    var bad_files: usize = 0;

    var lines = std.mem.splitScalar(u8, list, '\n');
    var n: usize = 0;
    while (lines.next()) |line| : (n += 1) {
        if (line.len == 0) continue;
        const path = if (std.mem.indexOfScalar(u8, line, '\t')) |tab| line[tab + 1 ..] else line;
        const src = try cwd.readFileAllocOptions(io, path, gpa, .unlimited, .of(u8), 0);
        defer gpa.free(src);

        const tok_path = try std.fmt.allocPrint(gpa, "{s}/tokens/{d}.tok", .{ work, n });
        defer gpa.free(tok_path);
        const toks = cwd.readFileAlloc(io, tok_path, gpa, .unlimited) catch |err| switch (err) {
            error.FileNotFound => {
                // tergi only lexes UTF-8, so it writes no tokens for other
                // files.
                if (std.unicode.utf8ValidateSlice(src)) {
                    try out.print("{s}: tergi wrote no tokens for it\n", .{path});
                    bad += 1;
                    bad_files += 1;
                } else {
                    skipped += 1;
                }
                continue;
            },
            else => return err,
        };
        defer gpa.free(toks);
        files += 1;
        bytes += src.len;

        const want = try gpa.alloc(?Kind, src.len);
        defer gpa.free(want);
        @memset(want, null);
        var want_idents: std.ArrayList(Range) = .empty;
        defer want_idents.deinit(gpa);
        var pos: usize = 0;
        var t = std.zig.Tokenizer.init(src);
        while (true) {
            const tok = t.next();
            if (tok.tag == .eof) break;
            const s = tok.loc.start;
            const e = tok.loc.end;
            fillGap(src, want, pos, s);
            const k = expected(tok.tag, src[s..e]);
            @memset(want[s..e], k);
            if (k == .Identifier) try want_idents.append(gpa, .{ .start = s, .end = e });
            pos = e;
        }
        fillGap(src, want, pos, src.len);

        const got = try gpa.alloc(?Kind, src.len);
        defer gpa.free(got);
        @memset(got, null);
        var got_idents: std.ArrayList(Range) = .empty;
        defer got_idents.deinit(gpa);
        var tok_lines = std.mem.tokenizeScalar(u8, toks, '\n');
        while (tok_lines.next()) |tl| {
            var fields = std.mem.tokenizeScalar(u8, tl, ' ');
            const s = try std.fmt.parseInt(usize, fields.next() orelse return error.BadTokens, 10);
            const e = try std.fmt.parseInt(usize, fields.next() orelse return error.BadTokens, 10);
            const name = fields.next() orelse return error.BadTokens;
            const k = std.meta.stringToEnum(Kind, name) orelse return error.BadTokens;
            if (s > e or e > src.len) return error.BadTokens;
            @memset(got[s..e], k);
            if (k == .Identifier) try got_idents.append(gpa, .{ .start = s, .end = e });
        }

        const bad_before = bad;
        var i: usize = 0;
        while (i < src.len) {
            const ok = got[i] == want[i] or
                (want[i] == .String and got[i] == .Escape) or
                // The tokenizer leaves the CR at the end of a comment or a
                // multiline string line outside its tokens, and tergi
                // colors it with the rest of the line.
                (src[i] == '\r' and want[i] == null);
            if (ok) {
                i += 1;
                continue;
            }
            bad += 1;
            const ls = if (std.mem.lastIndexOfScalar(u8, src[0..i], '\n')) |p| p + 1 else 0;
            const le = std.mem.indexOfScalarPos(u8, src, i, '\n') orelse src.len;
            if (bad <= max_shown) {
                const line_no = std.mem.count(u8, src[0..ls], "\n") + 1;
                try out.print("{s}:{d}:{d}: Zig wants {s}, tergi has {s}\n    {s}\n", .{
                    path, line_no, i - ls + 1, kindName(want[i]), kindName(got[i]), src[ls..le],
                });
            }
            // One disagreement per line is enough.
            i = le + 1;
        }

        if (!sameRanges(want_idents.items, got_idents.items)) {
            bad += 1;
            if (bad <= max_shown) try showIdents(out, path, src, want_idents.items, got_idents.items);
        }
        if (bad > bad_before) bad_files += 1;
    }

    try out.print(
        "{d} files ({d} bytes) compared, {d} skipped because they aren't UTF-8\n",
        .{ files, bytes, skipped },
    );
    try out.print("{d} disagreements in {d} files\n", .{ bad, bad_files });
    try out.flush();
    if (bad > 0) std.process.exit(1);
}

fn sameRanges(a: []const Range, b: []const Range) bool {
    if (a.len != b.len) return false;
    for (a, b) |x, y| {
        if (x.start != y.start or x.end != y.end) return false;
    }
    return true;
}

fn showIdents(out: *std.Io.Writer, path: []const u8, src: []const u8, want: []const Range, got: []const Range) !void {
    const len = @min(want.len, got.len);
    var i: usize = 0;
    while (i < len and want[i].start == got[i].start and want[i].end == got[i].end) i += 1;
    try out.print("{s}: identifiers differ, Zig has {d} and tergi {d}", .{ path, want.len, got.len });
    if (i < len) {
        const line_no = std.mem.count(u8, src[0..want[i].start], "\n") + 1;
        try out.print(". The first difference is on line {d}, where Zig has \"{s}\" and tergi \"{s}\"", .{
            line_no, src[want[i].start..want[i].end], src[got[i].start..got[i].end],
        });
    }
    try out.print("\n", .{});
}

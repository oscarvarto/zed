use crate::{
    MarkdownPreviewSettings,
    markdown_elements::{
        HeadingLevel, Image, Link, MarkdownParagraph, MarkdownParagraphChunk, ParsedMarkdown,
        ParsedMarkdownBlockQuote, ParsedMarkdownCodeBlock, ParsedMarkdownElement,
        ParsedMarkdownHeading, ParsedMarkdownListItem, ParsedMarkdownListItemType,
        ParsedMarkdownMath, ParsedMarkdownMathContents, ParsedMarkdownMathDisplayMode,
        ParsedMarkdownMermaidDiagram, ParsedMarkdownMermaidDiagramContents, ParsedMarkdownTable,
        ParsedMarkdownTableAlignment, ParsedMarkdownTableRow,
    },
    markdown_preview_view::MarkdownPreviewView,
};
use anyhow::Context as _;
use collections::HashMap;
use fs::normalize_path;
use gpui::{
    AbsoluteLength, Animation, AnimationExt, AnyElement, App, AppContext as _, Context, Div,
    Element, ElementId, Entity, HighlightStyle, Hsla, ImageSource, InteractiveText, IntoElement,
    Keystroke, Modifiers, ParentElement, Render, RenderImage, Resource, SharedString, Styled,
    StyledText, Task, TextStyle, WeakEntity, Window, div, img, pulsating_between, rems,
};
use settings::Settings;
use std::{
    ops::{Mul, Range},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, LazyLock, OnceLock},
    time::Duration,
    vec,
};
use tempfile::tempdir;
use theme::{ActiveTheme, SyntaxTheme, ThemeSettings};
use ui::{CopyButton, LinkPreview, ToggleState, prelude::*, tooltip_container};
use util::ResultExt as _;
use workspace::{OpenOptions, OpenVisible, Workspace};

pub struct CheckboxClickedEvent {
    pub checked: bool,
    pub source_range: Range<usize>,
}

impl CheckboxClickedEvent {
    pub fn source_range(&self) -> Range<usize> {
        self.source_range.clone()
    }

    pub fn checked(&self) -> bool {
        self.checked
    }
}

type CheckboxClickedCallback = Arc<Box<dyn Fn(&CheckboxClickedEvent, &mut Window, &mut App)>>;

type MermaidDiagramCache = HashMap<ParsedMarkdownMermaidDiagramContents, CachedMermaidDiagram>;
type MathFormulaCache = HashMap<ParsedMarkdownMathContents, CachedMathFormula>;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum MermaidRendererBackend {
    #[default]
    Rust,
    ExternalMmdc,
}

impl MermaidRendererBackend {
    fn from_settings(cx: &App) -> Self {
        if MarkdownPreviewSettings::try_get(cx)
            .is_some_and(|settings| settings.use_external_mermaid_mmdc)
        {
            Self::ExternalMmdc
        } else {
            Self::Rust
        }
    }
}

#[derive(Default)]
pub(crate) struct MermaidState {
    cache: MermaidDiagramCache,
    order: Vec<ParsedMarkdownMermaidDiagramContents>,
    renderer_backend: MermaidRendererBackend,
}

impl MermaidState {
    fn get_fallback_image(
        idx: usize,
        old_order: &[ParsedMarkdownMermaidDiagramContents],
        new_order_len: usize,
        cache: &MermaidDiagramCache,
    ) -> Option<Arc<RenderImage>> {
        // When the diagram count changes e.g. addition or removal, positional matching
        // is unreliable since a new diagram at index i likely doesn't correspond to the
        // old diagram at index i. We only allow fallbacks when counts match, which covers
        // the common case of editing a diagram in-place.
        //
        // Swapping two diagrams would briefly show the stale fallback, but that's an edge
        // case we don't handle.
        if old_order.len() != new_order_len {
            return None;
        }
        old_order.get(idx).and_then(|old_content| {
            cache.get(old_content).and_then(|old_cached| {
                old_cached
                    .render_image
                    .get()
                    .and_then(|result| result.as_ref().ok().cloned())
                    // Chain fallbacks for rapid edits.
                    .or_else(|| old_cached.fallback_image.clone())
            })
        })
    }

    fn set_renderer_backend(&mut self, renderer_backend: MermaidRendererBackend) -> bool {
        if self.renderer_backend == renderer_backend {
            return false;
        }

        self.cache.clear();
        self.order.clear();
        self.renderer_backend = renderer_backend;
        true
    }

    pub(crate) fn sync_renderer_backend(&mut self, cx: &App) -> bool {
        self.set_renderer_backend(MermaidRendererBackend::from_settings(cx))
    }

    pub(crate) fn update(
        &mut self,
        parsed: &ParsedMarkdown,
        cx: &mut Context<MarkdownPreviewView>,
    ) {
        use crate::markdown_elements::ParsedMarkdownElement;
        use std::collections::HashSet;

        self.sync_renderer_backend(cx);

        let mut new_order = Vec::new();
        for element in parsed.children.iter() {
            if let ParsedMarkdownElement::MermaidDiagram(mermaid_diagram) = element {
                new_order.push(mermaid_diagram.contents.clone());
            }
        }

        for (idx, new_content) in new_order.iter().enumerate() {
            if !self.cache.contains_key(new_content) {
                let fallback =
                    Self::get_fallback_image(idx, &self.order, new_order.len(), &self.cache);
                self.cache.insert(
                    new_content.clone(),
                    CachedMermaidDiagram::new(
                        new_content.clone(),
                        fallback,
                        self.renderer_backend,
                        cx,
                    ),
                );
            }
        }

        let new_order_set: HashSet<_> = new_order.iter().cloned().collect();
        self.cache
            .retain(|content, _| new_order_set.contains(content));
        self.order = new_order;
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct MathRenderStyle {
    font_size_millipoints: u32,
    text_color_rgb: u32,
}

impl MathRenderStyle {
    fn from_app(cx: &App) -> Self {
        let font_size_pixels: f32 = ThemeSettings::get_global(cx).buffer_font_size(cx).into();
        let font_size_points = font_size_pixels * 72.0 / 96.0;
        let rgba = cx.theme().colors().text.to_rgb();
        let red = (rgba.r * 255.0).round().clamp(0.0, 255.0) as u32;
        let green = (rgba.g * 255.0).round().clamp(0.0, 255.0) as u32;
        let blue = (rgba.b * 255.0).round().clamp(0.0, 255.0) as u32;

        Self {
            font_size_millipoints: (font_size_points * 1000.0).round().max(1.0) as u32,
            text_color_rgb: (red << 16) | (green << 8) | blue,
        }
    }

    fn font_size_points(self) -> f32 {
        self.font_size_millipoints as f32 / 1000.0
    }

    fn text_color_hex(self) -> String {
        format!("#{:06x}", self.text_color_rgb)
    }
}

#[derive(Default)]
pub(crate) struct MathState {
    cache: MathFormulaCache,
    order: Vec<ParsedMarkdownMathContents>,
    render_style: MathRenderStyle,
}

impl MathState {
    fn get_fallback_image(
        idx: usize,
        old_order: &[ParsedMarkdownMathContents],
        new_order_len: usize,
        cache: &MathFormulaCache,
    ) -> Option<Arc<RenderImage>> {
        if old_order.len() != new_order_len {
            return None;
        }

        old_order.get(idx).and_then(|old_content| {
            cache.get(old_content).and_then(|old_cached| {
                old_cached
                    .render_image
                    .get()
                    .and_then(|result| result.as_ref().ok().cloned())
                    .or_else(|| old_cached.fallback_image.clone())
            })
        })
    }

    fn set_render_style(&mut self, render_style: MathRenderStyle) -> bool {
        if self.render_style == render_style {
            return false;
        }

        self.cache.clear();
        self.order.clear();
        self.render_style = render_style;
        true
    }

    pub(crate) fn sync_render_style(&mut self, cx: &App) -> bool {
        self.set_render_style(MathRenderStyle::from_app(cx))
    }

    pub(crate) fn update(
        &mut self,
        parsed: &ParsedMarkdown,
        cx: &mut Context<MarkdownPreviewView>,
    ) {
        use std::collections::HashSet;

        self.sync_render_style(cx);

        let mut new_order = Vec::new();
        collect_math_contents(parsed, &mut new_order);

        for (idx, new_content) in new_order.iter().enumerate() {
            if !self.cache.contains_key(new_content) {
                let fallback =
                    Self::get_fallback_image(idx, &self.order, new_order.len(), &self.cache);
                self.cache.insert(
                    new_content.clone(),
                    CachedMathFormula::new(new_content.clone(), fallback, self.render_style, cx),
                );
            }
        }

        let new_order_set: HashSet<_> = new_order.iter().cloned().collect();
        self.cache
            .retain(|content, _| new_order_set.contains(content));
        self.order = new_order;
    }
}

pub(crate) struct CachedMermaidDiagram {
    pub(crate) render_image: Arc<OnceLock<anyhow::Result<Arc<RenderImage>>>>,
    pub(crate) fallback_image: Option<Arc<RenderImage>>,
    _task: Task<()>,
}

impl CachedMermaidDiagram {
    fn new(
        contents: ParsedMarkdownMermaidDiagramContents,
        fallback_image: Option<Arc<RenderImage>>,
        renderer_backend: MermaidRendererBackend,
        cx: &mut Context<MarkdownPreviewView>,
    ) -> Self {
        let result = Arc::new(OnceLock::<anyhow::Result<Arc<RenderImage>>>::new());
        let result_clone = result.clone();
        let svg_renderer = cx.svg_renderer();

        let _task = cx.spawn(async move |this, cx| {
            let value = cx
                .background_spawn(async move {
                    render_mermaid_diagram_image(&contents, renderer_backend, &svg_renderer).await
                })
                .await;
            if result_clone.set(value).is_err() {
                log::error!("mermaid render result was set more than once");
            }
            this.update(cx, |_, cx| {
                cx.notify();
            })
            .ok();
        });

        Self {
            render_image: result,
            fallback_image,
            _task,
        }
    }

    #[cfg(test)]
    fn new_for_test(
        render_image: Option<Arc<RenderImage>>,
        fallback_image: Option<Arc<RenderImage>>,
    ) -> Self {
        let result = Arc::new(OnceLock::new());
        if let Some(img) = render_image {
            assert!(result.set(Ok(img)).is_ok());
        }
        Self {
            render_image: result,
            fallback_image,
            _task: Task::ready(()),
        }
    }
}

pub(crate) struct CachedMathFormula {
    pub(crate) render_image: Arc<OnceLock<anyhow::Result<Arc<RenderImage>>>>,
    pub(crate) fallback_image: Option<Arc<RenderImage>>,
    _task: Task<()>,
}

impl CachedMathFormula {
    fn new(
        contents: ParsedMarkdownMathContents,
        fallback_image: Option<Arc<RenderImage>>,
        render_style: MathRenderStyle,
        cx: &mut Context<MarkdownPreviewView>,
    ) -> Self {
        let result = Arc::new(OnceLock::<anyhow::Result<Arc<RenderImage>>>::new());
        let result_clone = result.clone();
        let svg_renderer = cx.svg_renderer();

        let _task = cx.spawn(async move |this, cx| {
            let value = cx
                .background_spawn(async move {
                    render_math_formula_image(&contents, render_style, &svg_renderer)
                })
                .await;
            if result_clone.set(value).is_err() {
                log::error!("math render result was set more than once");
            }
            this.update(cx, |_, cx| {
                cx.notify();
            })
            .ok();
        });

        Self {
            render_image: result,
            fallback_image,
            _task,
        }
    }

    #[cfg(test)]
    fn new_for_test(
        render_image: Option<Arc<RenderImage>>,
        fallback_image: Option<Arc<RenderImage>>,
    ) -> Self {
        let result = Arc::new(OnceLock::new());
        if let Some(image) = render_image {
            assert!(result.set(Ok(image)).is_ok());
        }
        Self {
            render_image: result,
            fallback_image,
            _task: Task::ready(()),
        }
    }
}

async fn render_mermaid_diagram_image(
    contents: &ParsedMarkdownMermaidDiagramContents,
    renderer_backend: MermaidRendererBackend,
    svg_renderer: &gpui::SvgRenderer,
) -> anyhow::Result<Arc<RenderImage>> {
    let svg_bytes = match renderer_backend {
        MermaidRendererBackend::Rust => {
            mermaid_rs_renderer::render(&contents.contents)?.into_bytes()
        }
        MermaidRendererBackend::ExternalMmdc => render_mermaid_with_external_mmdc(contents).await?,
    };

    render_mermaid_svg(svg_renderer, &svg_bytes, contents.scale)
}

fn render_mermaid_svg(
    svg_renderer: &gpui::SvgRenderer,
    svg_bytes: &[u8],
    scale_percent: u32,
) -> anyhow::Result<Arc<RenderImage>> {
    let scale = scale_percent as f32 / 100.0;
    svg_renderer
        .render_single_frame(svg_bytes, scale, true)
        .map_err(|error| anyhow::anyhow!("{}", error))
}

async fn render_mermaid_with_external_mmdc(
    contents: &ParsedMarkdownMermaidDiagramContents,
) -> anyhow::Result<Vec<u8>> {
    let temporary_directory =
        tempdir().context("failed to create a temporary directory for Mermaid CLI rendering")?;
    let input_path = temporary_directory.path().join("diagram.mmd");
    let output_path = temporary_directory.path().join("diagram.svg");

    std::fs::write(&input_path, contents.contents.as_ref())
        .with_context(|| format!("failed to write Mermaid source to {}", input_path.display()))?;

    let mut command = util::command::new_command("mmdc");
    command
        .kill_on_drop(true)
        .arg("--input")
        .arg(&input_path)
        .arg("--output")
        .arg(&output_path);

    let output = command.output().await.context(
        "failed to execute Mermaid CLI renderer `mmdc`; ensure it is installed and available on PATH",
    )?;

    if !output.status.success() {
        let standard_error = String::from_utf8_lossy(&output.stderr);
        let standard_output = String::from_utf8_lossy(&output.stdout);
        let diagnostic = match (standard_error.trim(), standard_output.trim()) {
            ("", "") => String::new(),
            ("", stdout) => format!(": {stdout}"),
            (stderr, "") => format!(": {stderr}"),
            (stderr, stdout) => format!(": {stderr}; stdout: {stdout}"),
        };

        anyhow::bail!(
            "Mermaid CLI renderer `mmdc` exited with status {:?}{}",
            output.status.code(),
            diagnostic
        );
    }

    std::fs::read(&output_path).with_context(|| {
        format!(
            "failed to read Mermaid CLI output from {}",
            output_path.display()
        )
    })
}

fn collect_math_contents(parsed: &ParsedMarkdown, contents: &mut Vec<ParsedMarkdownMathContents>) {
    for child in &parsed.children {
        collect_math_contents_from_element(child, contents);
    }
}

fn collect_math_contents_from_element(
    element: &ParsedMarkdownElement,
    contents: &mut Vec<ParsedMarkdownMathContents>,
) {
    match element {
        ParsedMarkdownElement::Heading(heading) => {
            collect_math_contents_from_chunks(&heading.contents, contents);
        }
        ParsedMarkdownElement::ListItem(list_item) => {
            for child in &list_item.content {
                collect_math_contents_from_element(child, contents);
            }
        }
        ParsedMarkdownElement::Table(table) => {
            for row in table.header.iter().chain(table.body.iter()) {
                for column in &row.columns {
                    collect_math_contents_from_chunks(&column.children, contents);
                }
            }

            if let Some(caption) = table.caption.as_ref() {
                collect_math_contents_from_chunks(caption, contents);
            }
        }
        ParsedMarkdownElement::BlockQuote(block_quote) => {
            for child in &block_quote.children {
                collect_math_contents_from_element(child, contents);
            }
        }
        ParsedMarkdownElement::DisplayMath(math) => contents.push(math.contents.clone()),
        ParsedMarkdownElement::Paragraph(paragraph) => {
            collect_math_contents_from_chunks(paragraph, contents);
        }
        ParsedMarkdownElement::CodeBlock(_)
        | ParsedMarkdownElement::MermaidDiagram(_)
        | ParsedMarkdownElement::HorizontalRule(_)
        | ParsedMarkdownElement::Image(_) => {}
    }
}

fn collect_math_contents_from_chunks(
    paragraph: &MarkdownParagraph,
    contents: &mut Vec<ParsedMarkdownMathContents>,
) {
    for chunk in paragraph {
        if let MarkdownParagraphChunk::InlineMath(math) = chunk {
            contents.push(math.contents.clone());
        }
    }
}

fn render_math_formula_image(
    contents: &ParsedMarkdownMathContents,
    render_style: MathRenderStyle,
    svg_renderer: &gpui::SvgRenderer,
) -> anyhow::Result<Arc<RenderImage>> {
    let svg = render_math_formula_svg(contents, render_style)?;
    svg_renderer
        .render_single_frame(svg.as_bytes(), 1.0, true)
        .map_err(|error| anyhow::anyhow!("{}", error))
}

fn render_math_formula_svg(
    contents: &ParsedMarkdownMathContents,
    render_style: MathRenderStyle,
) -> anyhow::Result<String> {
    catch_unwind(AssertUnwindSafe(|| {
        render_math_formula_svg_inner(contents, render_style)
    }))
    .unwrap_or_else(|_| Err(anyhow::anyhow!("math rendering panicked")))
}

fn render_math_formula_svg_inner(
    contents: &ParsedMarkdownMathContents,
    render_style: MathRenderStyle,
) -> anyhow::Result<String> {
    let typst_math = mitex::convert_math(contents.contents.as_ref(), None)
        .map_err(|error| anyhow::anyhow!("failed to convert LaTeX math with mitex: {error}"))?;
    let typst_source = format!(
        "{MATH_TYPST_PREAMBLE}{}{formula}",
        math_typst_page_setup(contents.display_mode, render_style),
        formula = math_typst_formula_body(contents.display_mode, &typst_math),
    );

    compile_math_typst_to_svg(&typst_source)
}

fn math_typst_page_setup(
    display_mode: ParsedMarkdownMathDisplayMode,
    render_style: MathRenderStyle,
) -> String {
    let margin_points = match display_mode {
        ParsedMarkdownMathDisplayMode::Inline => 0.0,
        ParsedMarkdownMathDisplayMode::Display => 4.0,
    };

    format!(
        "#set page(width: auto, height: auto, margin: {margin_points:.3}pt, fill: none)\n\
         #set text(size: {:.3}pt, fill: rgb(\"{}\"))\n",
        render_style.font_size_points(),
        render_style.text_color_hex(),
    )
}

fn math_typst_formula_body(
    display_mode: ParsedMarkdownMathDisplayMode,
    typst_math: &str,
) -> String {
    match display_mode {
        ParsedMarkdownMathDisplayMode::Inline => format!("${typst_math}$"),
        ParsedMarkdownMathDisplayMode::Display => format!("$ {typst_math} $"),
    }
}

fn compile_math_typst_to_svg(source: &str) -> anyhow::Result<String> {
    use typst::layout::PagedDocument;

    let world = MathWorld::new(source);
    let warned = typst::compile::<PagedDocument>(&world);
    let document = warned.output.map_err(|diagnostics| {
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        anyhow::anyhow!("failed to compile Typst math document: {messages}")
    })?;

    let page = document
        .pages
        .first()
        .ok_or_else(|| anyhow::anyhow!("Typst did not produce a page for the math expression"))?;

    Ok(typst_svg::svg(page))
}

struct CachedMathFonts {
    book: typst::utils::LazyHash<typst::text::FontBook>,
    fonts: Vec<typst::text::Font>,
}

fn cached_math_fonts() -> &'static CachedMathFonts {
    static FONTS: LazyLock<CachedMathFonts> = LazyLock::new(|| {
        let mut book = typst::text::FontBook::new();
        let mut fonts = Vec::new();

        for data in typst_assets::fonts() {
            let bytes = typst::foundations::Bytes::new(data);
            for font in typst::text::Font::iter(bytes) {
                book.push(font.info().clone());
                fonts.push(font);
            }
        }

        CachedMathFonts {
            book: typst::utils::LazyHash::new(book),
            fonts,
        }
    });

    &FONTS
}

fn cached_math_library() -> &'static typst::utils::LazyHash<typst::Library> {
    static LIBRARY: LazyLock<typst::utils::LazyHash<typst::Library>> = LazyLock::new(|| {
        use typst::LibraryExt;

        typst::utils::LazyHash::new(typst::Library::default())
    });

    &LIBRARY
}

struct MathWorld {
    source: typst::syntax::Source,
}

impl MathWorld {
    fn new(source_text: &str) -> Self {
        Self {
            source: typst::syntax::Source::detached(source_text),
        }
    }
}

impl typst::World for MathWorld {
    fn library(&self) -> &typst::utils::LazyHash<typst::Library> {
        cached_math_library()
    }

    fn book(&self) -> &typst::utils::LazyHash<typst::text::FontBook> {
        &cached_math_fonts().book
    }

    fn main(&self) -> typst::syntax::FileId {
        self.source.id()
    }

    fn source(&self, id: typst::syntax::FileId) -> typst::diag::FileResult<typst::syntax::Source> {
        if id == self.source.id() {
            Ok(self.source.clone())
        } else {
            Err(typst::diag::FileError::AccessDenied)
        }
    }

    fn file(
        &self,
        _id: typst::syntax::FileId,
    ) -> typst::diag::FileResult<typst::foundations::Bytes> {
        Err(typst::diag::FileError::AccessDenied)
    }

    fn font(&self, index: usize) -> Option<typst::text::Font> {
        cached_math_fonts().fonts.get(index).cloned()
    }

    fn today(&self, _offset: Option<i64>) -> Option<typst::foundations::Datetime> {
        None
    }
}

const MATH_TYPST_PREAMBLE: &str = "\
#let textmath(it) = it\n\
#let textbf(it) = math.bold(it)\n\
#let textit(it) = math.italic(it)\n\
#let textmd(it) = it\n\
#let textnormal(it) = it\n\
#let textrm(it) = math.upright(it)\n\
#let textsf(it) = math.sans(it)\n\
#let texttt(it) = math.mono(it)\n\
#let textup(it) = math.upright(it)\n\
#let mitexmathbf(it) = math.bold(math.upright(it))\n\
#let mitexbold(it) = math.bold(math.upright(it))\n\
#let mitexupright(it) = math.upright(it)\n\
#let mitexitalic(it) = math.italic(it)\n\
#let mitexsans(it) = math.sans(it)\n\
#let mitexfrak(it) = math.frak(it)\n\
#let mitexmono(it) = math.mono(it)\n\
#let mitexcal(it) = math.cal(it)\n\
#let mitexdisplay(it) = math.display(it)\n\
#let mitexinline(it) = math.inline(it)\n\
#let mitexscript(it) = math.script(it)\n\
#let mitexsscript(it) = math.sscript(it)\n\
#let mitexsqrt(..args) = {\n\
  let positional_args = args.pos()\n\
  if positional_args.len() == 2 { math.root(positional_args.at(0), positional_args.at(1)) }\n\
  else if positional_args.len() > 0 { math.sqrt(positional_args.at(0)) }\n\
}\n\
#let pmatrix(..args) = math.mat(delim: \"(\", ..args)\n\
#let bmatrix(..args) = math.mat(delim: \"[\", ..args)\n\
#let Bmatrix(..args) = math.mat(delim: \"{\", ..args)\n\
#let vmatrix(..args) = math.mat(delim: \"|\", ..args)\n\
#let Vmatrix(..args) = math.mat(delim: \"||\", ..args)\n\
#let mitexarray(..args) = math.mat(..args)\n\
#let aligned(..args) = math.display(math.mat(delim: none, ..args))\n\
#let alignedat(..args) = math.display(math.mat(delim: none, ..args))\n\
#let rcases(..args) = math.cases(reverse: true, ..args)\n\
#let big(it) = math.lr(size: 1.2em, it)\n\
#let bigg(it) = math.lr(size: 2.4em, it)\n\
#let Big(it) = math.lr(size: 1.8em, it)\n\
#let Bigg(it) = math.lr(size: 3em, it)\n\
#let mitexoverbrace(..args) = math.overbrace(..args)\n\
#let mitexunderbrace(..args) = math.underbrace(..args)\n\
#let mitexoverbracket(..args) = math.overbracket(..args)\n\
#let mitexunderbracket(..args) = math.underbracket(..args)\n\
#let mitexcolor(it) = it\n\
#let colortext(..args) = {\n\
  let positional_args = args.pos()\n\
  if positional_args.len() >= 2 { positional_args.at(1) } else if positional_args.len() >= 1 { positional_args.at(0) }\n\
}\n\
#let operatornamewithlimits(it) = math.op(it, limits: true)\n\
#let atop(num, den) = math.frac(num, den)\n\
#let mitexcite(it) = it\n\
#let mitexref(it) = it\n\
#let mitexlabel(it) = none\n\
#let mitexcaption(it) = it\n\
#let miteximage(..args) = none\n\
#let bottomrule = none\n\
#let midrule = none\n\
#let toprule = none\n\
#let brace(it) = math.lr(size: auto, [{] + it + [}])\n\
#let brack(it) = math.lr(size: auto, $[$ + it + $]$)\n";

#[derive(Clone)]
pub struct RenderContext<'a> {
    workspace: Option<WeakEntity<Workspace>>,
    next_id: usize,
    buffer_font_family: SharedString,
    buffer_text_style: TextStyle,
    text_style: TextStyle,
    border_color: Hsla,
    title_bar_background_color: Hsla,
    panel_background_color: Hsla,
    text_color: Hsla,
    link_color: Hsla,
    window_rem_size: Pixels,
    text_muted_color: Hsla,
    code_block_background_color: Hsla,
    code_span_background_color: Hsla,
    syntax_theme: Arc<SyntaxTheme>,
    indent: usize,
    checkbox_clicked_callback: Option<CheckboxClickedCallback>,
    is_last_child: bool,
    mermaid_state: &'a MermaidState,
    math_state: &'a MathState,
}

impl<'a> RenderContext<'a> {
    pub(crate) fn new(
        workspace: Option<WeakEntity<Workspace>>,
        mermaid_state: &'a MermaidState,
        math_state: &'a MathState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        let theme = cx.theme().clone();

        let settings = ThemeSettings::get_global(cx);
        let buffer_font_family = settings.buffer_font.family.clone();
        let buffer_font_features = settings.buffer_font.features.clone();
        let mut buffer_text_style = window.text_style();
        buffer_text_style.font_family = buffer_font_family.clone();
        buffer_text_style.font_features = buffer_font_features;
        buffer_text_style.font_size = AbsoluteLength::from(settings.buffer_font_size(cx));

        RenderContext {
            workspace,
            next_id: 0,
            indent: 0,
            buffer_font_family,
            buffer_text_style,
            text_style: window.text_style(),
            syntax_theme: theme.syntax().clone(),
            border_color: theme.colors().border,
            title_bar_background_color: theme.colors().title_bar_background,
            panel_background_color: theme.colors().panel_background,
            text_color: theme.colors().text,
            link_color: theme.colors().text_accent,
            window_rem_size: window.rem_size(),
            text_muted_color: theme.colors().text_muted,
            code_block_background_color: theme.colors().surface_background,
            code_span_background_color: theme.colors().editor_document_highlight_read_background,
            checkbox_clicked_callback: None,
            is_last_child: false,
            mermaid_state,
            math_state,
        }
    }

    pub fn with_checkbox_clicked_callback(
        mut self,
        callback: impl Fn(&CheckboxClickedEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.checkbox_clicked_callback = Some(Arc::new(Box::new(callback)));
        self
    }

    fn next_id(&mut self, span: &Range<usize>) -> ElementId {
        let id = format!("markdown-{}-{}-{}", self.next_id, span.start, span.end);
        self.next_id += 1;
        ElementId::from(SharedString::from(id))
    }

    /// HACK: used to have rems relative to buffer font size, so that things scale appropriately as
    /// buffer font size changes. The callees of this function should be reimplemented to use real
    /// relative sizing once that is implemented in GPUI
    pub fn scaled_rems(&self, rems: f32) -> Rems {
        self.buffer_text_style
            .font_size
            .to_rems(self.window_rem_size)
            .mul(rems)
    }

    /// This ensures that children inside of block quotes
    /// have padding between them.
    ///
    /// For example, for this markdown:
    ///
    /// ```markdown
    /// > This is a block quote.
    /// >
    /// > And this is the next paragraph.
    /// ```
    ///
    /// We give padding between "This is a block quote."
    /// and "And this is the next paragraph."
    fn with_common_p(&self, element: Div) -> Div {
        if self.indent > 0 && !self.is_last_child {
            element.pb(self.scaled_rems(0.75))
        } else {
            element
        }
    }

    /// The is used to indicate that the current element is the last child or not of its parent.
    ///
    /// Then we can avoid adding padding to the bottom of the last child.
    fn with_last_child<R>(&mut self, is_last: bool, render: R) -> AnyElement
    where
        R: FnOnce(&mut Self) -> AnyElement,
    {
        self.is_last_child = is_last;
        let element = render(self);
        self.is_last_child = false;
        element
    }
}

pub fn render_parsed_markdown(
    parsed: &ParsedMarkdown,
    workspace: Option<WeakEntity<Workspace>>,
    window: &mut Window,
    cx: &mut App,
) -> Div {
    let mermaid_state = Default::default();
    let math_state = Default::default();
    let mut cx = RenderContext::new(workspace, &mermaid_state, &math_state, window, cx);

    v_flex().gap_3().children(
        parsed
            .children
            .iter()
            .map(|block| render_markdown_block(block, &mut cx)),
    )
}
pub fn render_markdown_block(block: &ParsedMarkdownElement, cx: &mut RenderContext) -> AnyElement {
    use ParsedMarkdownElement::*;
    match block {
        Paragraph(text) => render_markdown_paragraph(text, cx),
        Heading(heading) => render_markdown_heading(heading, cx),
        ListItem(list_item) => render_markdown_list_item(list_item, cx),
        Table(table) => render_markdown_table(table, cx),
        BlockQuote(block_quote) => render_markdown_block_quote(block_quote, cx),
        CodeBlock(code_block) => render_markdown_code_block(code_block, cx),
        MermaidDiagram(mermaid) => render_mermaid_diagram(mermaid, cx),
        DisplayMath(math) => render_display_math(math, cx),
        HorizontalRule(_) => render_markdown_rule(cx),
        Image(image) => render_markdown_image(image, cx),
    }
}

fn render_markdown_heading(parsed: &ParsedMarkdownHeading, cx: &mut RenderContext) -> AnyElement {
    let size = match parsed.level {
        HeadingLevel::H1 => 2.,
        HeadingLevel::H2 => 1.5,
        HeadingLevel::H3 => 1.25,
        HeadingLevel::H4 => 1.,
        HeadingLevel::H5 => 0.875,
        HeadingLevel::H6 => 0.85,
    };

    let text_size = cx.scaled_rems(size);

    // was `DefiniteLength::from(text_size.mul(1.25))`
    // let line_height = DefiniteLength::from(text_size.mul(1.25));
    let line_height = text_size * 1.25;

    // was `rems(0.15)`
    // let padding_top = cx.scaled_rems(0.15);
    let padding_top = rems(0.15);

    // was `.pb_1()` = `rems(0.25)`
    // let padding_bottom = cx.scaled_rems(0.25);
    let padding_bottom = rems(0.25);

    let color = match parsed.level {
        HeadingLevel::H6 => cx.text_muted_color,
        _ => cx.text_color,
    };
    div()
        .line_height(line_height)
        .text_size(text_size)
        .text_color(color)
        .pt(padding_top)
        .pb(padding_bottom)
        .child(render_inline_markdown(&parsed.contents, cx))
        .whitespace_normal()
        .into_any()
}

fn render_markdown_list_item(
    parsed: &ParsedMarkdownListItem,
    cx: &mut RenderContext,
) -> AnyElement {
    use ParsedMarkdownListItemType::*;
    let depth = parsed.depth.saturating_sub(1) as usize;

    let bullet = match &parsed.item_type {
        Ordered(order) => list_item_prefix(*order as usize, true, depth).into_any_element(),
        Unordered => list_item_prefix(1, false, depth).into_any_element(),
        Task(checked, range) => div()
            .id(cx.next_id(range))
            .mt(cx.scaled_rems(3.0 / 16.0))
            .child(
                MarkdownCheckbox::new(
                    "checkbox",
                    if *checked {
                        ToggleState::Selected
                    } else {
                        ToggleState::Unselected
                    },
                    cx.clone(),
                )
                .when_some(
                    cx.checkbox_clicked_callback.clone(),
                    |this, callback| {
                        this.on_click({
                            let range = range.clone();
                            move |selection, window, cx| {
                                let checked = match selection {
                                    ToggleState::Selected => true,
                                    ToggleState::Unselected => false,
                                    _ => return,
                                };

                                if window.modifiers().secondary() {
                                    callback(
                                        &CheckboxClickedEvent {
                                            checked,
                                            source_range: range.clone(),
                                        },
                                        window,
                                        cx,
                                    );
                                }
                            }
                        })
                    },
                ),
            )
            .hover(|s| s.cursor_pointer())
            .tooltip(|_, cx| {
                InteractiveMarkdownElementTooltip::new(None, "toggle checkbox", cx).into()
            })
            .into_any_element(),
    };
    let bullet = div().mr(cx.scaled_rems(0.5)).child(bullet);

    let contents: Vec<AnyElement> = parsed
        .content
        .iter()
        .map(|c| render_markdown_block(c, cx))
        .collect();

    let item = h_flex()
        .when(!parsed.nested, |this| this.pl(cx.scaled_rems(depth as f32)))
        .when(parsed.nested && depth > 0, |this| this.ml_neg_1p5())
        .items_start()
        .children(vec![
            bullet,
            v_flex()
                .children(contents)
                .when(!parsed.nested, |this| this.gap(cx.scaled_rems(1.0)))
                .pr(cx.scaled_rems(1.0))
                .w_full(),
        ]);

    cx.with_common_p(item).into_any()
}

/// # MarkdownCheckbox ///
/// HACK: Copied from `ui/src/components/toggle.rs` to deal with scaling issues in markdown preview
/// changes should be integrated into `Checkbox` in `toggle.rs` while making sure checkboxes elsewhere in the
/// app are not visually affected
#[derive(gpui::IntoElement)]
struct MarkdownCheckbox {
    id: ElementId,
    toggle_state: ToggleState,
    disabled: bool,
    placeholder: bool,
    on_click: Option<Box<dyn Fn(&ToggleState, &mut Window, &mut App) + 'static>>,
    filled: bool,
    style: ui::ToggleStyle,
    tooltip: Option<Box<dyn Fn(&mut Window, &mut App) -> gpui::AnyView>>,
    label: Option<SharedString>,
    base_rem: Rems,
}

impl MarkdownCheckbox {
    /// Creates a new [`Checkbox`].
    fn new(id: impl Into<ElementId>, checked: ToggleState, render_cx: RenderContext) -> Self {
        Self {
            id: id.into(),
            toggle_state: checked,
            disabled: false,
            on_click: None,
            filled: false,
            style: ui::ToggleStyle::default(),
            tooltip: None,
            label: None,
            placeholder: false,
            base_rem: render_cx.scaled_rems(1.0),
        }
    }

    /// Binds a handler to the [`Checkbox`] that will be called when clicked.
    fn on_click(mut self, handler: impl Fn(&ToggleState, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }

    fn bg_color(&self, cx: &App) -> Hsla {
        let style = self.style.clone();
        match (style, self.filled) {
            (ui::ToggleStyle::Ghost, false) => cx.theme().colors().ghost_element_background,
            (ui::ToggleStyle::Ghost, true) => cx.theme().colors().element_background,
            (ui::ToggleStyle::ElevationBased(_), false) => gpui::transparent_black(),
            (ui::ToggleStyle::ElevationBased(elevation), true) => elevation.darker_bg(cx),
            (ui::ToggleStyle::Custom(_), false) => gpui::transparent_black(),
            (ui::ToggleStyle::Custom(color), true) => color.opacity(0.2),
        }
    }

    fn border_color(&self, cx: &App) -> Hsla {
        if self.disabled {
            return cx.theme().colors().border_variant;
        }

        match self.style.clone() {
            ui::ToggleStyle::Ghost => cx.theme().colors().border,
            ui::ToggleStyle::ElevationBased(_) => cx.theme().colors().border,
            ui::ToggleStyle::Custom(color) => color.opacity(0.3),
        }
    }
}

impl gpui::RenderOnce for MarkdownCheckbox {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let group_id = format!("checkbox_group_{:?}", self.id);
        let color = if self.disabled {
            Color::Disabled
        } else {
            Color::Selected
        };
        let icon_size_small = IconSize::Custom(self.base_rem.mul(14. / 16.)); // was IconSize::Small
        let icon = match self.toggle_state {
            ToggleState::Selected => {
                if self.placeholder {
                    None
                } else {
                    Some(
                        ui::Icon::new(IconName::Check)
                            .size(icon_size_small)
                            .color(color),
                    )
                }
            }
            ToggleState::Indeterminate => Some(
                ui::Icon::new(IconName::Dash)
                    .size(icon_size_small)
                    .color(color),
            ),
            ToggleState::Unselected => None,
        };

        let bg_color = self.bg_color(cx);
        let border_color = self.border_color(cx);
        let hover_border_color = border_color.alpha(0.7);

        let size = self.base_rem.mul(1.25); // was Self::container_size(); (20px)

        let checkbox = h_flex()
            .id(self.id.clone())
            .justify_center()
            .items_center()
            .size(size)
            .group(group_id.clone())
            .child(
                div()
                    .flex()
                    .flex_none()
                    .justify_center()
                    .items_center()
                    .m(self.base_rem.mul(0.25)) // was .m_1
                    .size(self.base_rem.mul(1.0)) // was .size_4
                    .rounded(self.base_rem.mul(0.125)) // was .rounded_xs
                    .border_1()
                    .bg(bg_color)
                    .border_color(border_color)
                    .when(self.disabled, |this| this.cursor_not_allowed())
                    .when(self.disabled, |this| {
                        this.bg(cx.theme().colors().element_disabled.opacity(0.6))
                    })
                    .when(!self.disabled, |this| {
                        this.group_hover(group_id.clone(), |el| el.border_color(hover_border_color))
                    })
                    .when(self.placeholder, |this| {
                        this.child(
                            div()
                                .flex_none()
                                .rounded_full()
                                .bg(color.color(cx).alpha(0.5))
                                .size(self.base_rem.mul(0.25)), // was .size_1
                        )
                    })
                    .children(icon),
            );

        h_flex()
            .id(self.id)
            .gap(ui::DynamicSpacing::Base06.rems(cx))
            .child(checkbox)
            .when_some(
                self.on_click.filter(|_| !self.disabled),
                |this, on_click| {
                    this.on_click(move |_, window, cx| {
                        on_click(&self.toggle_state.inverse(), window, cx)
                    })
                },
            )
            // TODO: Allow label size to be different from default.
            // TODO: Allow label color to be different from muted.
            .when_some(self.label, |this, label| {
                this.child(Label::new(label).color(Color::Muted))
            })
            .when_some(self.tooltip, |this, tooltip| {
                this.tooltip(move |window, cx| tooltip(window, cx))
            })
    }
}

fn calculate_table_columns_count(rows: &Vec<ParsedMarkdownTableRow>) -> usize {
    let mut actual_column_count = 0;
    for row in rows {
        actual_column_count = actual_column_count.max(
            row.columns
                .iter()
                .map(|column| column.col_span)
                .sum::<usize>(),
        );
    }
    actual_column_count
}

fn render_markdown_table(parsed: &ParsedMarkdownTable, cx: &mut RenderContext) -> AnyElement {
    let actual_header_column_count = calculate_table_columns_count(&parsed.header);
    let actual_body_column_count = calculate_table_columns_count(&parsed.body);
    let max_column_count = std::cmp::max(actual_header_column_count, actual_body_column_count);

    let total_rows = parsed.header.len() + parsed.body.len();

    // Track which grid cells are occupied by spanning cells
    let mut grid_occupied = vec![vec![false; max_column_count]; total_rows];

    let mut cells = Vec::with_capacity(total_rows * max_column_count);

    for (row_idx, row) in parsed.header.iter().chain(parsed.body.iter()).enumerate() {
        let mut col_idx = 0;

        for cell in row.columns.iter() {
            // Skip columns occupied by row-spanning cells from previous rows
            while col_idx < max_column_count && grid_occupied[row_idx][col_idx] {
                col_idx += 1;
            }

            if col_idx >= max_column_count {
                break;
            }

            let container = match cell.alignment {
                ParsedMarkdownTableAlignment::Left | ParsedMarkdownTableAlignment::None => div(),
                ParsedMarkdownTableAlignment::Center => v_flex().items_center(),
                ParsedMarkdownTableAlignment::Right => v_flex().items_end(),
            };

            let cell_element = container
                .col_span(cell.col_span.min(max_column_count - col_idx) as u16)
                .row_span(cell.row_span.min(total_rows - row_idx) as u16)
                .child(render_inline_markdown(&cell.children, cx))
                .px_2()
                .py_1()
                .when(col_idx > 0, |this| this.border_l_1())
                .when(row_idx > 0, |this| this.border_t_1())
                .border_color(cx.border_color)
                .when(cell.is_header, |this| {
                    this.bg(cx.title_bar_background_color)
                })
                .when(cell.row_span > 1, |this| this.justify_center())
                .when(row_idx % 2 == 1, |this| this.bg(cx.panel_background_color));

            cells.push(cell_element);

            // Mark grid positions as occupied for row-spanning cells
            for r in 0..cell.row_span {
                for c in 0..cell.col_span {
                    if row_idx + r < total_rows && col_idx + c < max_column_count {
                        grid_occupied[row_idx + r][col_idx + c] = true;
                    }
                }
            }

            col_idx += cell.col_span;
        }

        // Fill remaining columns with empty cells if needed
        while col_idx < max_column_count {
            if grid_occupied[row_idx][col_idx] {
                col_idx += 1;
                continue;
            }

            let empty_cell = div()
                .when(col_idx > 0, |this| this.border_l_1())
                .when(row_idx > 0, |this| this.border_t_1())
                .border_color(cx.border_color)
                .when(row_idx % 2 == 1, |this| this.bg(cx.panel_background_color));

            cells.push(empty_cell);
            col_idx += 1;
        }
    }

    cx.with_common_p(v_flex().items_start())
        .when_some(parsed.caption.as_ref(), |this, caption| {
            this.child(render_inline_markdown(caption, cx))
        })
        .border_1()
        .border_color(cx.border_color)
        .rounded_sm()
        .overflow_hidden()
        .child(
            div()
                .min_w_0()
                .w_full()
                .grid()
                .grid_cols(max_column_count as u16)
                .children(cells),
        )
        .into_any()
}

fn render_markdown_block_quote(
    parsed: &ParsedMarkdownBlockQuote,
    cx: &mut RenderContext,
) -> AnyElement {
    cx.indent += 1;

    let children: Vec<AnyElement> = parsed
        .children
        .iter()
        .enumerate()
        .map(|(ix, child)| {
            cx.with_last_child(ix + 1 == parsed.children.len(), |cx| {
                render_markdown_block(child, cx)
            })
        })
        .collect();

    cx.indent -= 1;

    cx.with_common_p(div())
        .child(
            div()
                .border_l_4()
                .border_color(cx.border_color)
                .pl_3()
                .children(children),
        )
        .into_any()
}

fn render_markdown_code_block(
    parsed: &ParsedMarkdownCodeBlock,
    cx: &mut RenderContext,
) -> AnyElement {
    let body = if let Some(highlights) = parsed.highlights.as_ref() {
        StyledText::new(parsed.contents.clone()).with_default_highlights(
            &cx.buffer_text_style,
            highlights.iter().filter_map(|(range, highlight_id)| {
                highlight_id
                    .style(cx.syntax_theme.as_ref())
                    .map(|style| (range.clone(), style))
            }),
        )
    } else {
        StyledText::new(parsed.contents.clone())
    };

    let copy_block_button = CopyButton::new("copy-codeblock", parsed.contents.clone())
        .tooltip_label("Copy Codeblock")
        .visible_on_hover("markdown-block");

    let font = gpui::Font {
        family: cx.buffer_font_family.clone(),
        features: cx.buffer_text_style.font_features.clone(),
        ..Default::default()
    };

    cx.with_common_p(div())
        .font(font)
        .px_3()
        .py_3()
        .bg(cx.code_block_background_color)
        .rounded_sm()
        .child(body)
        .child(
            div()
                .h_flex()
                .absolute()
                .right_1()
                .top_1()
                .child(copy_block_button),
        )
        .into_any()
}

fn render_mermaid_diagram(
    parsed: &ParsedMarkdownMermaidDiagram,
    cx: &mut RenderContext,
) -> AnyElement {
    let cached = cx.mermaid_state.cache.get(&parsed.contents);

    if let Some(result) = cached.and_then(|c| c.render_image.get()) {
        match result {
            Ok(render_image) => cx
                .with_common_p(div())
                .px_3()
                .py_3()
                .bg(cx.code_block_background_color)
                .rounded_sm()
                .child(
                    div().w_full().child(
                        img(ImageSource::Render(render_image.clone()))
                            .max_w_full()
                            .with_fallback(|| {
                                div()
                                    .child(Label::new("Failed to load mermaid diagram"))
                                    .into_any_element()
                            }),
                    ),
                )
                .into_any(),
            Err(_) => cx
                .with_common_p(div())
                .px_3()
                .py_3()
                .bg(cx.code_block_background_color)
                .rounded_sm()
                .child(StyledText::new(parsed.contents.contents.clone()))
                .into_any(),
        }
    } else if let Some(fallback) = cached.and_then(|c| c.fallback_image.as_ref()) {
        cx.with_common_p(div())
            .px_3()
            .py_3()
            .bg(cx.code_block_background_color)
            .rounded_sm()
            .child(
                div()
                    .w_full()
                    .child(
                        img(ImageSource::Render(fallback.clone()))
                            .max_w_full()
                            .with_fallback(|| {
                                div()
                                    .child(Label::new("Failed to load mermaid diagram"))
                                    .into_any_element()
                            }),
                    )
                    .with_animation(
                        "mermaid-fallback-pulse",
                        Animation::new(Duration::from_secs(2))
                            .repeat()
                            .with_easing(pulsating_between(0.6, 1.0)),
                        |el, delta| el.opacity(delta),
                    ),
            )
            .into_any()
    } else {
        cx.with_common_p(div())
            .px_3()
            .py_3()
            .bg(cx.code_block_background_color)
            .rounded_sm()
            .child(
                Label::new("Rendering mermaid diagram...")
                    .color(Color::Muted)
                    .with_animation(
                        "mermaid-loading-pulse",
                        Animation::new(Duration::from_secs(2))
                            .repeat()
                            .with_easing(pulsating_between(0.4, 0.8)),
                        |label, delta| label.alpha(delta),
                    ),
            )
            .into_any()
    }
}

fn render_markdown_paragraph(parsed: &MarkdownParagraph, cx: &mut RenderContext) -> AnyElement {
    cx.with_common_p(div())
        .child(render_inline_markdown(parsed, cx))
        .into_any_element()
}

fn render_inline_markdown(parsed: &MarkdownParagraph, cx: &mut RenderContext) -> AnyElement {
    let container = div().children(render_markdown_text(parsed, cx));
    if parsed
        .iter()
        .any(|chunk| !matches!(chunk, MarkdownParagraphChunk::Text(_)))
    {
        container
            .flex()
            .flex_row()
            .flex_wrap()
            .items_baseline()
            .into_any_element()
    } else {
        container.into_any_element()
    }
}

fn render_display_math(parsed: &ParsedMarkdownMath, cx: &mut RenderContext) -> AnyElement {
    let cached = cx.math_state.cache.get(&parsed.contents);
    let body =
        if let Some(result) = cached.and_then(|cached_formula| cached_formula.render_image.get()) {
            match result {
                Ok(render_image) => div()
                    .w_full()
                    .flex()
                    .justify_center()
                    .child(
                        img(ImageSource::Render(render_image.clone()))
                            .max_w_full()
                            .with_fallback(|| {
                                div()
                                    .child(Label::new("Failed to load math expression"))
                                    .into_any_element()
                            }),
                    )
                    .into_any_element(),
                Err(_) => div()
                    .w_full()
                    .child(StyledText::new(math_fallback_text(&parsed.contents)))
                    .into_any_element(),
            }
        } else if let Some(fallback) =
            cached.and_then(|cached_formula| cached_formula.fallback_image.as_ref())
        {
            div()
                .w_full()
                .flex()
                .justify_center()
                .child(
                    img(ImageSource::Render(fallback.clone()))
                        .max_w_full()
                        .with_fallback(|| {
                            div()
                                .child(Label::new("Failed to load math expression"))
                                .into_any_element()
                        }),
                )
                .with_animation(
                    "math-display-fallback-pulse",
                    Animation::new(Duration::from_secs(2))
                        .repeat()
                        .with_easing(pulsating_between(0.6, 1.0)),
                    |element, delta| element.opacity(delta),
                )
                .into_any_element()
        } else {
            div()
                .w_full()
                .flex()
                .justify_center()
                .child(
                    Label::new("Rendering math...")
                        .color(Color::Muted)
                        .with_animation(
                            "math-display-loading-pulse",
                            Animation::new(Duration::from_secs(2))
                                .repeat()
                                .with_easing(pulsating_between(0.4, 0.8)),
                            |label, delta| label.alpha(delta),
                        ),
                )
                .into_any_element()
        };

    cx.with_common_p(div())
        .py(cx.scaled_rems(0.5))
        .child(body)
        .into_any_element()
}

fn math_fallback_text(contents: &ParsedMarkdownMathContents) -> SharedString {
    match contents.display_mode {
        ParsedMarkdownMathDisplayMode::Inline => format!("${}$", contents.contents).into(),
        ParsedMarkdownMathDisplayMode::Display => format!("$$ {} $$", contents.contents).into(),
    }
}

fn open_markdown_link(
    link: &Link,
    workspace: &Option<WeakEntity<Workspace>>,
    window: &mut Window,
    cx: &mut App,
) {
    match link {
        Link::Web { url } => cx.open_url(url),
        Link::Path { path, .. } => {
            if let Some(workspace) = workspace {
                let normalized_path = normalize_path(path.clone().as_path());
                workspace
                    .update(cx, |workspace, cx| {
                        workspace
                            .open_abs_path(
                                normalized_path,
                                OpenOptions {
                                    visible: Some(OpenVisible::None),
                                    ..Default::default()
                                },
                                window,
                                cx,
                            )
                            .detach();
                    })
                    .log_err();
            }
        }
    }
}

fn render_markdown_text(parsed_new: &MarkdownParagraph, cx: &mut RenderContext) -> Vec<AnyElement> {
    let mut any_element = Vec::with_capacity(parsed_new.len());
    // these values are cloned in-order satisfy borrow checker
    let syntax_theme = cx.syntax_theme.clone();
    let workspace_clone = cx.workspace.clone();
    let code_span_bg_color = cx.code_span_background_color;
    let text_style = cx.text_style.clone();
    let link_color = cx.link_color;

    for parsed_region in parsed_new {
        match parsed_region {
            MarkdownParagraphChunk::Text(parsed) => {
                let element_id = cx.next_id(&parsed.source_range);

                let highlights = gpui::combine_highlights(
                    parsed.highlights.iter().filter_map(|(range, highlight)| {
                        highlight
                            .to_highlight_style(&syntax_theme)
                            .map(|style| (range.clone(), style))
                    }),
                    parsed.regions.iter().filter_map(|(range, region)| {
                        if region.code {
                            Some((
                                range.clone(),
                                HighlightStyle {
                                    background_color: Some(code_span_bg_color),
                                    ..Default::default()
                                },
                            ))
                        } else if region.link.is_some() {
                            Some((
                                range.clone(),
                                HighlightStyle {
                                    color: Some(link_color),
                                    ..Default::default()
                                },
                            ))
                        } else {
                            None
                        }
                    }),
                );
                let mut links = Vec::new();
                let mut link_ranges = Vec::new();
                for (range, region) in parsed.regions.iter() {
                    if let Some(link) = region.link.clone() {
                        links.push(link);
                        link_ranges.push(range.clone());
                    }
                }
                let workspace = workspace_clone.clone();
                let element = div()
                    .child(
                        InteractiveText::new(
                            element_id,
                            StyledText::new(parsed.contents.clone())
                                .with_default_highlights(&text_style, highlights),
                        )
                        .tooltip({
                            let links = links.clone();
                            let link_ranges = link_ranges.clone();
                            move |idx, _, cx| {
                                for (ix, range) in link_ranges.iter().enumerate() {
                                    if range.contains(&idx) {
                                        return Some(LinkPreview::new(&links[ix].to_string(), cx));
                                    }
                                }
                                None
                            }
                        })
                        .on_click(
                            link_ranges,
                            move |clicked_range_ix, window, cx| {
                                open_markdown_link(&links[clicked_range_ix], &workspace, window, cx)
                            },
                        ),
                    )
                    .into_any();
                any_element.push(element);
            }

            MarkdownParagraphChunk::InlineMath(math) => {
                let math_body = {
                    let cached = cx.math_state.cache.get(&math.contents);
                    if let Some(result) =
                        cached.and_then(|cached_formula| cached_formula.render_image.get())
                    {
                        match result {
                            Ok(render_image) => img(ImageSource::Render(render_image.clone()))
                                .with_fallback({
                                    let fallback_text = math_fallback_text(&math.contents);
                                    move || {
                                        div()
                                            .child(StyledText::new(fallback_text.clone()))
                                            .into_any_element()
                                    }
                                })
                                .into_any_element(),
                            Err(_) => div()
                                .child(StyledText::new(math_fallback_text(&math.contents)))
                                .into_any_element(),
                        }
                    } else if let Some(fallback) =
                        cached.and_then(|cached_formula| cached_formula.fallback_image.as_ref())
                    {
                        img(ImageSource::Render(fallback.clone()))
                            .with_fallback({
                                let fallback_text = math_fallback_text(&math.contents);
                                move || {
                                    div()
                                        .child(StyledText::new(fallback_text.clone()))
                                        .into_any_element()
                                }
                            })
                            .with_animation(
                                "math-inline-fallback-pulse",
                                Animation::new(Duration::from_secs(2))
                                    .repeat()
                                    .with_easing(pulsating_between(0.6, 1.0)),
                                |element, delta| element.opacity(delta),
                            )
                            .into_any_element()
                    } else {
                        div()
                            .child(
                                Label::new(math_fallback_text(&math.contents))
                                    .color(Color::Muted)
                                    .with_animation(
                                        "math-inline-loading-pulse",
                                        Animation::new(Duration::from_secs(2))
                                            .repeat()
                                            .with_easing(pulsating_between(0.4, 0.8)),
                                        |label, delta| label.alpha(delta),
                                    ),
                            )
                            .into_any_element()
                    }
                };

                let workspace = workspace_clone.clone();
                let element_id = cx.next_id(&math.source_range);
                let element = div()
                    .id(element_id)
                    .when(math.link.is_some(), |element| element.cursor_pointer())
                    .when_some(math.link.clone(), |element, link| {
                        element.tooltip(move |_, cx| {
                            InteractiveMarkdownElementTooltip::new(
                                Some(link.to_string().into()),
                                "open link",
                                cx,
                            )
                            .into()
                        })
                    })
                    .when_some(math.link.clone(), |element, link| {
                        element.on_click(move |_, window, cx| {
                            open_markdown_link(&link, &workspace, window, cx)
                        })
                    })
                    .child(math_body)
                    .into_any_element();
                any_element.push(element);
            }

            MarkdownParagraphChunk::Image(image) => {
                any_element.push(render_markdown_image(image, cx));
            }
        }
    }

    any_element
}

fn render_markdown_rule(cx: &mut RenderContext) -> AnyElement {
    let rule = div().w_full().h(cx.scaled_rems(0.125)).bg(cx.border_color);
    div().py(cx.scaled_rems(0.5)).child(rule).into_any()
}

fn render_markdown_image(image: &Image, cx: &mut RenderContext) -> AnyElement {
    let image_resource = match image.link.clone() {
        Link::Web { url } => Resource::Uri(url.into()),
        Link::Path { path, .. } => Resource::Path(Arc::from(path)),
    };

    let element_id = cx.next_id(&image.source_range);
    let workspace = cx.workspace.clone();

    div()
        .id(element_id)
        .cursor_pointer()
        .child(
            img(ImageSource::Resource(image_resource))
                .max_w_full()
                .with_fallback({
                    let alt_text = image.alt_text.clone();
                    move || div().children(alt_text.clone()).into_any_element()
                })
                .when_some(image.height, |this, height| this.h(height))
                .when_some(image.width, |this, width| this.w(width)),
        )
        .tooltip({
            let link = image.link.clone();
            let alt_text = image.alt_text.clone();
            move |_, cx| {
                InteractiveMarkdownElementTooltip::new(
                    Some(alt_text.clone().unwrap_or(link.to_string().into())),
                    "open image",
                    cx,
                )
                .into()
            }
        })
        .on_click({
            let link = image.link.clone();
            move |_, window, cx| {
                if window.modifiers().secondary() {
                    match &link {
                        Link::Web { url } => cx.open_url(url),
                        Link::Path { path, .. } => {
                            if let Some(workspace) = &workspace {
                                _ = workspace.update(cx, |workspace, cx| {
                                    workspace
                                        .open_abs_path(
                                            path.clone(),
                                            OpenOptions {
                                                visible: Some(OpenVisible::None),
                                                ..Default::default()
                                            },
                                            window,
                                            cx,
                                        )
                                        .detach();
                                });
                            }
                        }
                    }
                }
            }
        })
        .into_any()
}

struct InteractiveMarkdownElementTooltip {
    tooltip_text: Option<SharedString>,
    action_text: SharedString,
}

impl InteractiveMarkdownElementTooltip {
    pub fn new(
        tooltip_text: Option<SharedString>,
        action_text: impl Into<SharedString>,
        cx: &mut App,
    ) -> Entity<Self> {
        let tooltip_text = tooltip_text.map(|t| util::truncate_and_trailoff(&t, 50).into());

        cx.new(|_cx| Self {
            tooltip_text,
            action_text: action_text.into(),
        })
    }
}

impl Render for InteractiveMarkdownElementTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        tooltip_container(cx, |el, _| {
            let secondary_modifier = Keystroke {
                modifiers: Modifiers::secondary_key(),
                ..Default::default()
            };

            el.child(
                v_flex()
                    .gap_1()
                    .when_some(self.tooltip_text.clone(), |this, text| {
                        this.child(Label::new(text).size(LabelSize::Small))
                    })
                    .child(
                        Label::new(format!(
                            "{}-click to {}",
                            secondary_modifier, self.action_text
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            )
        })
    }
}

/// Returns the prefix for a list item.
fn list_item_prefix(order: usize, ordered: bool, depth: usize) -> String {
    let ix = order.saturating_sub(1);
    const NUMBERED_PREFIXES_1: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    const NUMBERED_PREFIXES_2: &str = "abcdefghijklmnopqrstuvwxyz";
    const BULLETS: [&str; 5] = ["•", "◦", "▪", "‣", "⁃"];

    if ordered {
        match depth {
            0 => format!("{}. ", order),
            1 => format!(
                "{}. ",
                NUMBERED_PREFIXES_1
                    .chars()
                    .nth(ix % NUMBERED_PREFIXES_1.len())
                    .unwrap()
            ),
            _ => format!(
                "{}. ",
                NUMBERED_PREFIXES_2
                    .chars()
                    .nth(ix % NUMBERED_PREFIXES_2.len())
                    .unwrap()
            ),
        }
    } else {
        let depth = depth.min(BULLETS.len() - 1);
        let bullet = BULLETS[depth];
        return format!("{} ", bullet);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown_elements::ParsedMarkdownMermaidDiagramContents;
    use crate::markdown_elements::ParsedMarkdownTableColumn;
    use crate::markdown_elements::ParsedMarkdownText;

    fn text(text: &str) -> MarkdownParagraphChunk {
        MarkdownParagraphChunk::Text(ParsedMarkdownText {
            source_range: 0..text.len(),
            contents: SharedString::new(text),
            highlights: Default::default(),
            regions: Default::default(),
        })
    }

    fn column(
        col_span: usize,
        row_span: usize,
        children: Vec<MarkdownParagraphChunk>,
    ) -> ParsedMarkdownTableColumn {
        ParsedMarkdownTableColumn {
            col_span,
            row_span,
            is_header: false,
            children,
            alignment: ParsedMarkdownTableAlignment::None,
        }
    }

    fn column_with_row_span(
        col_span: usize,
        row_span: usize,
        children: Vec<MarkdownParagraphChunk>,
    ) -> ParsedMarkdownTableColumn {
        ParsedMarkdownTableColumn {
            col_span,
            row_span,
            is_header: false,
            children,
            alignment: ParsedMarkdownTableAlignment::None,
        }
    }

    #[test]
    fn test_calculate_table_columns_count() {
        assert_eq!(0, calculate_table_columns_count(&vec![]));

        assert_eq!(
            1,
            calculate_table_columns_count(&vec![ParsedMarkdownTableRow::with_columns(vec![
                column(1, 1, vec![text("column1")])
            ])])
        );

        assert_eq!(
            2,
            calculate_table_columns_count(&vec![ParsedMarkdownTableRow::with_columns(vec![
                column(1, 1, vec![text("column1")]),
                column(1, 1, vec![text("column2")]),
            ])])
        );

        assert_eq!(
            2,
            calculate_table_columns_count(&vec![ParsedMarkdownTableRow::with_columns(vec![
                column(2, 1, vec![text("column1")])
            ])])
        );

        assert_eq!(
            3,
            calculate_table_columns_count(&vec![ParsedMarkdownTableRow::with_columns(vec![
                column(1, 1, vec![text("column1")]),
                column(2, 1, vec![text("column2")]),
            ])])
        );

        assert_eq!(
            2,
            calculate_table_columns_count(&vec![
                ParsedMarkdownTableRow::with_columns(vec![
                    column(1, 1, vec![text("column1")]),
                    column(1, 1, vec![text("column2")]),
                ]),
                ParsedMarkdownTableRow::with_columns(vec![column(1, 1, vec![text("column1")]),])
            ])
        );

        assert_eq!(
            3,
            calculate_table_columns_count(&vec![
                ParsedMarkdownTableRow::with_columns(vec![
                    column(1, 1, vec![text("column1")]),
                    column(1, 1, vec![text("column2")]),
                ]),
                ParsedMarkdownTableRow::with_columns(vec![column(3, 3, vec![text("column1")]),])
            ])
        );
    }

    #[test]
    fn test_row_span_support() {
        assert_eq!(
            3,
            calculate_table_columns_count(&vec![
                ParsedMarkdownTableRow::with_columns(vec![
                    column_with_row_span(1, 2, vec![text("spans 2 rows")]),
                    column(1, 1, vec![text("column2")]),
                    column(1, 1, vec![text("column3")]),
                ]),
                ParsedMarkdownTableRow::with_columns(vec![
                    // First column is covered by row span from above
                    column(1, 1, vec![text("column2 row2")]),
                    column(1, 1, vec![text("column3 row2")]),
                ])
            ])
        );

        assert_eq!(
            4,
            calculate_table_columns_count(&vec![
                ParsedMarkdownTableRow::with_columns(vec![
                    column_with_row_span(1, 3, vec![text("spans 3 rows")]),
                    column_with_row_span(2, 1, vec![text("spans 2 cols")]),
                    column(1, 1, vec![text("column4")]),
                ]),
                ParsedMarkdownTableRow::with_columns(vec![
                    // First column covered by row span
                    column(1, 1, vec![text("column2")]),
                    column(1, 1, vec![text("column3")]),
                    column(1, 1, vec![text("column4")]),
                ]),
                ParsedMarkdownTableRow::with_columns(vec![
                    // First column still covered by row span
                    column(3, 1, vec![text("spans 3 cols")]),
                ])
            ])
        );
    }

    #[test]
    fn test_list_item_prefix() {
        assert_eq!(list_item_prefix(1, true, 0), "1. ");
        assert_eq!(list_item_prefix(2, true, 0), "2. ");
        assert_eq!(list_item_prefix(3, true, 0), "3. ");
        assert_eq!(list_item_prefix(11, true, 0), "11. ");
        assert_eq!(list_item_prefix(1, true, 1), "A. ");
        assert_eq!(list_item_prefix(2, true, 1), "B. ");
        assert_eq!(list_item_prefix(3, true, 1), "C. ");
        assert_eq!(list_item_prefix(1, true, 2), "a. ");
        assert_eq!(list_item_prefix(2, true, 2), "b. ");
        assert_eq!(list_item_prefix(7, true, 2), "g. ");
        assert_eq!(list_item_prefix(1, true, 1), "A. ");
        assert_eq!(list_item_prefix(1, true, 2), "a. ");
        assert_eq!(list_item_prefix(1, false, 0), "• ");
        assert_eq!(list_item_prefix(1, false, 1), "◦ ");
        assert_eq!(list_item_prefix(1, false, 2), "▪ ");
        assert_eq!(list_item_prefix(1, false, 3), "‣ ");
        assert_eq!(list_item_prefix(1, false, 4), "⁃ ");
    }

    fn mermaid_contents(s: &str) -> ParsedMarkdownMermaidDiagramContents {
        ParsedMarkdownMermaidDiagramContents {
            contents: SharedString::from(s.to_string()),
            scale: 1,
        }
    }

    fn math_contents(
        contents: &str,
        display_mode: ParsedMarkdownMathDisplayMode,
    ) -> ParsedMarkdownMathContents {
        ParsedMarkdownMathContents {
            contents: SharedString::from(contents.to_string()),
            display_mode,
        }
    }

    fn mermaid_sequence(diagrams: &[&str]) -> Vec<ParsedMarkdownMermaidDiagramContents> {
        diagrams
            .iter()
            .map(|diagram| mermaid_contents(diagram))
            .collect()
    }

    fn math_sequence(
        formulas: &[(&str, ParsedMarkdownMathDisplayMode)],
    ) -> Vec<ParsedMarkdownMathContents> {
        formulas
            .iter()
            .map(|(formula, display_mode)| math_contents(formula, *display_mode))
            .collect()
    }

    fn mermaid_fallback(
        new_diagram: &str,
        new_full_order: &[ParsedMarkdownMermaidDiagramContents],
        old_full_order: &[ParsedMarkdownMermaidDiagramContents],
        cache: &MermaidDiagramCache,
    ) -> Option<Arc<RenderImage>> {
        let new_content = mermaid_contents(new_diagram);
        let idx = new_full_order
            .iter()
            .position(|content| content == &new_content)?;
        MermaidState::get_fallback_image(idx, old_full_order, new_full_order.len(), cache)
    }

    fn mock_render_image() -> Arc<RenderImage> {
        Arc::new(RenderImage::new(Vec::new()))
    }

    #[test]
    fn test_mermaid_cache_is_cleared_when_renderer_backend_changes() {
        let mut state = MermaidState {
            cache: HashMap::default(),
            order: mermaid_sequence(&["graph A"]),
            renderer_backend: MermaidRendererBackend::Rust,
        };
        state.cache.insert(
            mermaid_contents("graph A"),
            CachedMermaidDiagram::new_for_test(Some(mock_render_image()), None),
        );

        assert!(state.set_renderer_backend(MermaidRendererBackend::ExternalMmdc));
        assert!(state.cache.is_empty());
        assert!(state.order.is_empty());
    }

    #[test]
    fn test_math_cache_is_cleared_when_render_style_changes() {
        let mut state = MathState {
            cache: HashMap::default(),
            order: math_sequence(&[("x^2", ParsedMarkdownMathDisplayMode::Inline)]),
            render_style: MathRenderStyle {
                font_size_millipoints: 12000,
                text_color_rgb: 0xffffff,
            },
        };
        state.cache.insert(
            math_contents("x^2", ParsedMarkdownMathDisplayMode::Inline),
            CachedMathFormula::new_for_test(Some(mock_render_image()), None),
        );

        assert!(state.set_render_style(MathRenderStyle {
            font_size_millipoints: 14000,
            text_color_rgb: 0xffffff,
        }));
        assert!(state.cache.is_empty());
        assert!(state.order.is_empty());
    }

    #[test]
    fn test_math_fallback_on_edit() {
        let old_full_order = math_sequence(&[
            ("x", ParsedMarkdownMathDisplayMode::Inline),
            ("x^2", ParsedMarkdownMathDisplayMode::Display),
            ("y", ParsedMarkdownMathDisplayMode::Inline),
        ]);
        let new_full_order = math_sequence(&[
            ("x", ParsedMarkdownMathDisplayMode::Inline),
            ("x^3", ParsedMarkdownMathDisplayMode::Display),
            ("y", ParsedMarkdownMathDisplayMode::Inline),
        ]);

        let svg = mock_render_image();
        let mut cache: MathFormulaCache = HashMap::default();
        cache.insert(
            math_contents("x", ParsedMarkdownMathDisplayMode::Inline),
            CachedMathFormula::new_for_test(Some(mock_render_image()), None),
        );
        cache.insert(
            math_contents("x^2", ParsedMarkdownMathDisplayMode::Display),
            CachedMathFormula::new_for_test(Some(svg.clone()), None),
        );
        cache.insert(
            math_contents("y", ParsedMarkdownMathDisplayMode::Inline),
            CachedMathFormula::new_for_test(Some(mock_render_image()), None),
        );

        let new_formula = math_contents("x^3", ParsedMarkdownMathDisplayMode::Display);
        let idx = new_full_order
            .iter()
            .position(|contents| contents == &new_formula)
            .unwrap();
        let fallback =
            MathState::get_fallback_image(idx, &old_full_order, new_full_order.len(), &cache);

        assert!(
            fallback.is_some(),
            "Should use old formula as fallback when editing"
        );
        assert!(
            Arc::ptr_eq(&fallback.unwrap(), &svg),
            "Fallback should be the old formula's SVG"
        );
    }

    #[test]
    fn test_render_math_formula_svg() {
        let svg = render_math_formula_svg(
            &math_contents("e^{i\\pi} + 1 = 0", ParsedMarkdownMathDisplayMode::Inline),
            MathRenderStyle {
                font_size_millipoints: 12000,
                text_color_rgb: 0xffffff,
            },
        )
        .expect("math should render to SVG");

        assert!(svg.contains("<svg"), "output should be valid SVG");
    }

    #[test]
    fn test_render_math_formula_svg_invalid_formula_is_error() {
        let result = render_math_formula_svg(
            &math_contents(
                "}}}{{{\\invalid\\command",
                ParsedMarkdownMathDisplayMode::Inline,
            ),
            MathRenderStyle {
                font_size_millipoints: 12000,
                text_color_rgb: 0xffffff,
            },
        );

        assert!(result.is_err(), "invalid math should return an error");
    }

    #[test]
    fn test_mermaid_fallback_on_edit() {
        let old_full_order = mermaid_sequence(&["graph A", "graph B", "graph C"]);
        let new_full_order = mermaid_sequence(&["graph A", "graph B modified", "graph C"]);

        let svg_b = mock_render_image();
        let mut cache: MermaidDiagramCache = HashMap::default();
        cache.insert(
            mermaid_contents("graph A"),
            CachedMermaidDiagram::new_for_test(Some(mock_render_image()), None),
        );
        cache.insert(
            mermaid_contents("graph B"),
            CachedMermaidDiagram::new_for_test(Some(svg_b.clone()), None),
        );
        cache.insert(
            mermaid_contents("graph C"),
            CachedMermaidDiagram::new_for_test(Some(mock_render_image()), None),
        );

        let fallback =
            mermaid_fallback("graph B modified", &new_full_order, &old_full_order, &cache);

        assert!(
            fallback.is_some(),
            "Should use old diagram as fallback when editing"
        );
        assert!(
            Arc::ptr_eq(&fallback.unwrap(), &svg_b),
            "Fallback should be the old diagram's SVG"
        );
    }

    #[test]
    fn test_mermaid_no_fallback_on_add_in_middle() {
        let old_full_order = mermaid_sequence(&["graph A", "graph C"]);
        let new_full_order = mermaid_sequence(&["graph A", "graph NEW", "graph C"]);

        let mut cache: MermaidDiagramCache = HashMap::default();
        cache.insert(
            mermaid_contents("graph A"),
            CachedMermaidDiagram::new_for_test(Some(mock_render_image()), None),
        );
        cache.insert(
            mermaid_contents("graph C"),
            CachedMermaidDiagram::new_for_test(Some(mock_render_image()), None),
        );

        let fallback = mermaid_fallback("graph NEW", &new_full_order, &old_full_order, &cache);

        assert!(
            fallback.is_none(),
            "Should NOT use fallback when adding new diagram"
        );
    }

    #[test]
    fn test_mermaid_fallback_chains_on_rapid_edits() {
        let old_full_order = mermaid_sequence(&["graph A", "graph B modified", "graph C"]);
        let new_full_order = mermaid_sequence(&["graph A", "graph B modified again", "graph C"]);

        let original_svg = mock_render_image();
        let mut cache: MermaidDiagramCache = HashMap::default();
        cache.insert(
            mermaid_contents("graph A"),
            CachedMermaidDiagram::new_for_test(Some(mock_render_image()), None),
        );
        cache.insert(
            mermaid_contents("graph B modified"),
            // Still rendering, but has fallback from original "graph B"
            CachedMermaidDiagram::new_for_test(None, Some(original_svg.clone())),
        );
        cache.insert(
            mermaid_contents("graph C"),
            CachedMermaidDiagram::new_for_test(Some(mock_render_image()), None),
        );

        let fallback = mermaid_fallback(
            "graph B modified again",
            &new_full_order,
            &old_full_order,
            &cache,
        );

        assert!(
            fallback.is_some(),
            "Should chain fallback when previous render not complete"
        );
        assert!(
            Arc::ptr_eq(&fallback.unwrap(), &original_svg),
            "Fallback should chain through to the original SVG"
        );
    }

    #[test]
    fn test_mermaid_no_fallback_when_no_old_diagram_at_index() {
        let old_full_order = mermaid_sequence(&["graph A"]);
        let new_full_order = mermaid_sequence(&["graph A", "graph B"]);

        let mut cache: MermaidDiagramCache = HashMap::default();
        cache.insert(
            mermaid_contents("graph A"),
            CachedMermaidDiagram::new_for_test(Some(mock_render_image()), None),
        );

        let fallback = mermaid_fallback("graph B", &new_full_order, &old_full_order, &cache);

        assert!(
            fallback.is_none(),
            "Should NOT have fallback when adding diagram at end"
        );
    }

    #[test]
    fn test_mermaid_fallback_with_duplicate_blocks_edit_first() {
        let old_full_order = mermaid_sequence(&["graph A", "graph A", "graph B"]);
        let new_full_order = mermaid_sequence(&["graph A edited", "graph A", "graph B"]);

        let svg_a = mock_render_image();
        let mut cache: MermaidDiagramCache = HashMap::default();
        cache.insert(
            mermaid_contents("graph A"),
            CachedMermaidDiagram::new_for_test(Some(svg_a.clone()), None),
        );
        cache.insert(
            mermaid_contents("graph B"),
            CachedMermaidDiagram::new_for_test(Some(mock_render_image()), None),
        );

        let fallback = mermaid_fallback("graph A edited", &new_full_order, &old_full_order, &cache);

        assert!(
            fallback.is_some(),
            "Should use old diagram as fallback when editing one of duplicate blocks"
        );
        assert!(
            Arc::ptr_eq(&fallback.unwrap(), &svg_a),
            "Fallback should be the old duplicate diagram's image"
        );
    }

    #[test]
    fn test_mermaid_fallback_with_duplicate_blocks_edit_second() {
        let old_full_order = mermaid_sequence(&["graph A", "graph A", "graph B"]);
        let new_full_order = mermaid_sequence(&["graph A", "graph A edited", "graph B"]);

        let svg_a = mock_render_image();
        let mut cache: MermaidDiagramCache = HashMap::default();
        cache.insert(
            mermaid_contents("graph A"),
            CachedMermaidDiagram::new_for_test(Some(svg_a.clone()), None),
        );
        cache.insert(
            mermaid_contents("graph B"),
            CachedMermaidDiagram::new_for_test(Some(mock_render_image()), None),
        );

        let fallback = mermaid_fallback("graph A edited", &new_full_order, &old_full_order, &cache);

        assert!(
            fallback.is_some(),
            "Should use old diagram as fallback when editing the second duplicate block"
        );
        assert!(
            Arc::ptr_eq(&fallback.unwrap(), &svg_a),
            "Fallback should be the old duplicate diagram's image"
        );
    }
}

// Page extraction (roadmap 9.x). Evaluated in the page as
// `(<this function>)(args)`; returns plain JSON.
//
// Modes: summary, article, tables, items, links, next, click_next, watch.
// Only rendered text is read (innerText), never field values, so passwords
// and card numbers typed into inputs don't come back.
(function (args) {
  const mode = args.mode;
  const MAX = args.max_chars || 20000;
  const clean = (t) => (t || '').replace(/\s+/g, ' ').trim();
  const abs = (h) => {
    try { return new URL(h, location.href).href; } catch (e) { return h || ''; }
  };
  const visible = (el) => {
    if (!el || !el.getClientRects || !el.getClientRects().length) return false;
    const s = getComputedStyle(el);
    return s.visibility !== 'hidden' && s.display !== 'none' && parseFloat(s.opacity || '1') > 0.05;
  };
  const text = (el) => clean(el ? (el.innerText || '') : '');
  const meta = (n) => {
    const m = document.querySelector(`meta[property="${n}"], meta[name="${n}"], meta[itemprop="${n}"]`);
    return m ? clean(m.getAttribute('content')) : '';
  };
  const PRICE = /(?:US\$|C\$|A\$|[$€£¥₹]|USD|EUR|GBP|CAD|AUD)\s?\d[\d,]*(?:\.\d{1,2})?|\d[\d.,]*\d\s?(?:USD|EUR|GBP|€|kr|zł)/;
  const PRICE_G = new RegExp(PRICE.source, 'g');
  const SKIP = 'nav, footer, header, aside, [role=navigation], [role=banner], [role=contentinfo], [role=complementary], [aria-hidden=true], script, style, noscript, svg';
  const JUNK = /\b(share|social|comment|related|promo|sidebar|footer|newsletter|subscribe|advert|ad-slot|breadcrumb|cookie|consent|popup|modal)\b/i;

  // ------------------------------------------------------------ structured data
  function jsonLd() {
    const out = [];
    for (const s of document.querySelectorAll('script[type="application/ld+json"]')) {
      let v;
      try { v = JSON.parse(s.textContent); } catch (e) { continue; }
      const stack = [v];
      while (stack.length && out.length < 50) {
        const o = stack.shift();
        if (!o || typeof o !== 'object') continue;
        if (Array.isArray(o)) { stack.push(...o); continue; }
        if (o['@graph']) stack.push(...[].concat(o['@graph']));
        if (o['@type']) out.push(o);
      }
    }
    return out;
  }
  const types = (o) => [].concat(o['@type'] || []).map(String);
  const str = (v) => {
    if (v == null) return '';
    if (typeof v === 'object') return clean(v.name || v['@id'] || '');
    return clean(String(v));
  };
  const schemaWord = (v) => str(v).replace(/^https?:\/\/schema\.org\//, '');

  function productFrom(o) {
    const offers = [].concat(o.offers || []);
    const off = offers[0] || {};
    const price = off.price ?? off.lowPrice ?? (off.priceSpecification || {}).price;
    const r = o.aggregateRating || {};
    return {
      name: str(o.name),
      brand: str(o.brand),
      sku: str(o.sku || o.mpn || o.gtin13 || o.gtin12),
      price: price != null ? String(price) : '',
      high_price: off.highPrice != null ? String(off.highPrice) : '',
      currency: str(off.priceCurrency || (off.priceSpecification || {}).priceCurrency),
      availability: schemaWord(off.availability),
      condition: schemaWord(off.itemCondition),
      seller: str(off.seller),
      rating: r.ratingValue != null ? String(r.ratingValue) : '',
      best_rating: r.bestRating != null ? String(r.bestRating) : '',
      reviews: String(r.reviewCount ?? r.ratingCount ?? ''),
      offers: offers.length,
    };
  }

  function microdataProduct() {
    const scope = document.querySelector('[itemtype*="schema.org/Product"]');
    if (!scope) return null;
    const prop = (n) => {
      const el = scope.querySelector(`[itemprop="${n}"]`);
      if (!el) return '';
      return clean(el.getAttribute('content') || el.getAttribute('href') || el.innerText);
    };
    return {
      name: prop('name'), brand: prop('brand'), sku: prop('sku'), price: prop('price'), high_price: '',
      currency: prop('priceCurrency'), availability: schemaWord(prop('availability')), condition: '',
      seller: '', rating: prop('ratingValue'), best_rating: prop('bestRating'), reviews: prop('reviewCount'), offers: 0,
    };
  }

  function articleMeta(ld) {
    const a = ld.find((o) => types(o).some((t) => /Article|BlogPosting|NewsArticle|Report/.test(t))) || {};
    const author = [].concat(a.author || []).map(str).filter(Boolean).join(', ');
    const authorEl = document.querySelector('[rel=author], [itemprop=author], .byline, .author, [class*="byline"]');
    const timeEl = document.querySelector('time[datetime]');
    return {
      headline: str(a.headline),
      author: author || meta('author') || meta('article:author') || (authorEl && visible(authorEl) ? text(authorEl).slice(0, 120) : ''),
      published: str(a.datePublished) || meta('article:published_time') || (timeEl ? timeEl.getAttribute('datetime') : ''),
      modified: str(a.dateModified) || meta('article:modified_time'),
    };
  }

  // Prices shown near the main heading, else anywhere in the main content.
  function visiblePrices() {
    const found = [];
    const add = (root) => {
      if (!root) return;
      const t = root.innerText || '';
      for (const m of t.matchAll(PRICE_G)) {
        const p = clean(m[0]);
        if (!found.includes(p)) found.push(p);
        if (found.length >= 6) return;
      }
    };
    const h1 = [...document.querySelectorAll('h1')].find(visible);
    if (h1) {
      let box = h1;
      for (let i = 0; i < 4 && box.parentElement; i++) {
        box = box.parentElement;
        if ((box.innerText || '').match(PRICE)) break;
      }
      add(box);
    }
    if (!found.length) add(document.querySelector('main, [role=main]') || document.body);
    return found;
  }

  // Key/value pairs from definition lists and two-column tables.
  function specs(limit) {
    const out = [];
    const push = (k, v) => {
      k = clean(k).replace(/:$/, ''); v = clean(v);
      if (k && v && k.length < 80 && v.length < 300 && !out.some((p) => p[0] === k)) out.push([k, v]);
    };
    for (const dl of document.querySelectorAll('dl')) {
      if (!visible(dl) || dl.closest(SKIP)) continue;
      for (const dt of dl.querySelectorAll('dt')) {
        const dd = dt.nextElementSibling;
        if (dd && dd.tagName === 'DD') push(text(dt), text(dd));
        if (out.length >= limit) return out;
      }
    }
    for (const tr of document.querySelectorAll('table tr')) {
      if (tr.closest(SKIP) || !visible(tr)) continue;
      const cells = [...tr.children].filter((c) => /^T[HD]$/.test(c.tagName));
      if (cells.length === 2) push(text(cells[0]), text(cells[1]));
      if (out.length >= limit) return out;
    }
    return out;
  }

  function summary() {
    const ld = jsonLd();
    const p = ld.find((o) => types(o).some((t) => /^(Product|ProductGroup|Book|Offer|SoftwareApplication)$/.test(t)));
    const h1 = [...document.querySelectorAll('h1')].find(visible);
    const main = document.querySelector('main, [role=main], article') || document.body;
    return {
      url: location.href,
      title: document.title,
      heading: text(h1),
      description: meta('description') || meta('og:description'),
      site_name: meta('og:site_name'),
      product: p ? productFrom(p) : microdataProduct(),
      article: articleMeta(ld),
      prices: visiblePrices(),
      specs: specs(args.max_specs || 40),
      excerpt: text(main).slice(0, args.excerpt_chars || 1500),
    };
  }

  // ------------------------------------------------------------ article text
  function article() {
    const ld = jsonLd();
    const scores = new Map();
    for (const p of document.querySelectorAll('p, pre, blockquote')) {
      if (p.closest(SKIP)) continue;
      const len = (p.innerText || '').trim().length;
      if (len < 40) continue;
      const parent = p.parentElement;
      const grand = parent && parent.parentElement;
      const commas = ((p.innerText || '').match(/,/g) || []).length;
      const score = Math.min(len, 1000) / 100 + commas + 1;
      if (parent) scores.set(parent, (scores.get(parent) || 0) + score);
      if (grand) scores.set(grand, (scores.get(grand) || 0) + score / 2);
    }
    let best = null, bestScore = 0;
    for (const [el, s] of scores) {
      const hint = (el.className || '') + ' ' + (el.id || '');
      const adj = JUNK.test(hint) ? s * 0.3 : (/article|content|post|entry|story|body|text/i.test(hint) ? s * 1.3 : s);
      if (adj > bestScore) { best = el; bestScore = adj; }
    }
    const root = best || document.querySelector('article, main, [role=main]') || document.body;
    const lines = [];
    let size = 0, truncated = false;
    const walk = (el) => {
      if (truncated) return;
      for (const c of el.children) {
        if (truncated) return;
        if (c.matches(SKIP) || !visible(c)) continue;
        const hint = (c.className && c.className.baseVal === undefined ? c.className : '') + ' ' + (c.id || '');
        if (JUNK.test(hint) && c !== root) continue;
        const tag = c.tagName;
        let line = null;
        if (/^H[1-6]$/.test(tag)) line = '#'.repeat(Math.min(+tag[1] + 1, 4)) + ' ' + text(c);
        else if (tag === 'P' || tag === 'BLOCKQUOTE' || tag === 'FIGCAPTION') line = (tag === 'BLOCKQUOTE' ? '> ' : '') + text(c);
        else if (tag === 'LI') line = '- ' + text(c);
        else if (tag === 'PRE') line = '```\n' + (c.innerText || '').trim() + '\n```';
        else if (tag === 'TABLE') line = '(table: ' + clean(text(c)).slice(0, 200) + '…)';
        else if (tag === 'IMG' && c.alt) line = '[image: ' + clean(c.alt) + ']';
        if (line !== null) {
          if (line.replace(/^[#>\- ]+/, '').length < 2) continue;
          if (size + line.length > MAX) { truncated = true; return; }
          lines.push(line); size += line.length + 1;
        } else {
          walk(c);
        }
      }
    };
    walk(root);
    const body = lines.join('\n');
    return {
      url: location.href,
      title: document.title,
      ...articleMeta(ld),
      words: body.split(/\s+/).filter(Boolean).length,
      text: body,
      truncated,
    };
  }

  // ------------------------------------------------------------ tables
  function heading_before(el) {
    let n = el;
    for (let depth = 0; n && depth < 4; depth++, n = n.parentElement) {
      let s = n.previousElementSibling;
      for (let i = 0; s && i < 4; i++, s = s.previousElementSibling) {
        if (/^H[1-6]$/.test(s.tagName)) return text(s);
        const h = s.querySelector && s.querySelector('h1,h2,h3,h4,h5,h6');
        if (h) return text(h);
      }
    }
    return '';
  }

  function tables() {
    const maxRows = args.max_rows || 50;
    const out = [];
    const els = [...document.querySelectorAll('table, [role=table], [role=grid]')];
    for (const t of els) {
      if (out.length >= (args.max_tables || 10)) break;
      if (!visible(t) || (t.tagName === 'TABLE' && t.querySelector('table'))) continue;
      if (t.tagName !== 'TABLE' && t.closest('table')) continue;
      const rowEls = t.tagName === 'TABLE' ? [...t.rows] : [...t.querySelectorAll('[role=row]')];
      const rows = rowEls.filter(visible).map((r) => {
        const cells = t.tagName === 'TABLE'
          ? [...r.cells]
          : [...r.querySelectorAll('[role=cell], [role=gridcell], [role=columnheader], [role=rowheader]')];
        return { header: cells.length > 0 && cells.every((c) => c.tagName === 'TH' || /header/.test(c.getAttribute('role') || '')), cells: cells.map((c) => text(c).slice(0, 300)) };
      }).filter((r) => r.cells.some(Boolean));
      if (rows.length < 2) continue;
      const width = Math.max(...rows.map((r) => r.cells.length));
      if (width < 2) continue;
      let headers = [];
      if (rows[0].header) headers = rows.shift().cells;
      const caption = t.tagName === 'TABLE' && t.caption ? text(t.caption) : (t.getAttribute('aria-label') || heading_before(t));
      out.push({ caption, headers, rows: rows.slice(0, maxRows).map((r) => r.cells), total_rows: rows.length });
    }
    return { url: location.href, title: document.title, tables: out };
  }

  // ------------------------------------------------------------ repeated items
  // Search results and product grids: the container with the most children
  // that look alike and each carry a link.
  function signature(el) {
    const cls = [...el.classList].filter((c) => !/\d/.test(c)).sort().slice(0, 3).join('.');
    return el.tagName + '.' + cls;
  }

  function itemOf(el) {
    const links = [...el.querySelectorAll('a[href]')].filter((a) => visible(a) && !/^(javascript:|#)/.test(a.getAttribute('href')));
    if (!links.length) return null;
    const h = [...el.querySelectorAll('h1,h2,h3,h4,h5,[role=heading]')].find(visible);
    let link = h && (h.closest('a[href]') || h.querySelector('a[href]'));
    if (!link) link = links.reduce((a, b) => (text(b).length > text(a).length ? b : a));
    const title = text(h) || text(link) || clean(link.getAttribute('aria-label') || link.title);
    if (!title) return null;
    const all = text(el);
    const price = (all.match(PRICE) || [''])[0];
    let rating = '';
    const star = el.querySelector('[aria-label*="star" i], [aria-label*="rating" i], [title*="star" i]');
    if (star) rating = clean(star.getAttribute('aria-label') || star.title).slice(0, 60);
    if (!rating) {
      const m = all.match(/(\d(?:\.\d)?)\s*(?:out of 5|\/\s*5|stars?)/i);
      if (m) rating = m[0];
    }
    let snippet = all.replace(title, '');
    if (price) snippet = snippet.replace(price, '');
    if (rating) snippet = snippet.replace(rating, '');
    // Separators left behind ("Bass nut ·  · stars") collapse to one.
    snippet = snippet.replace(/(\s*[·|•]\s*){2,}/g, ' · ').replace(/^[\s·|•-]+|[\s·|•-]+$/g, '').trim();
    return { title: title.slice(0, 200), url: abs(link.getAttribute('href')), price, rating, text: snippet.slice(0, args.snippet_chars || 220) };
  }

  function items() {
    let best = null, bestScore = 0;
    for (const parent of document.querySelectorAll('body *')) {
      if (parent.children.length < 3 || parent.closest('nav, footer, header, [role=navigation]')) continue;
      const groups = new Map();
      for (const c of parent.children) {
        const sig = signature(c);
        if (!groups.has(sig)) groups.set(sig, []);
        groups.get(sig).push(c);
      }
      for (const [, kids] of groups) {
        if (kids.length < 3) continue;
        const sample = kids.slice(0, 8);
        let withLink = 0, len = 0, priced = 0;
        for (const k of sample) {
          if (k.querySelector('a[href]')) withLink++;
          const t = (k.textContent || '').replace(/\s+/g, ' ').trim();
          len += Math.min(t.length, 300);
          if (PRICE.test(t)) priced++;
        }
        if (withLink < sample.length * 0.6 || len / sample.length < 15) continue;
        const score = kids.length * (len / sample.length) * (1 + priced / sample.length);
        if (score > bestScore) { bestScore = score; best = kids; }
      }
    }
    const list = [];
    const seen = new Set();
    for (const k of best || []) {
      if (!visible(k)) continue;
      const it = itemOf(k);
      if (!it || seen.has(it.url)) continue;
      seen.add(it.url);
      list.push(it);
      if (list.length >= (args.max_items || 60)) break;
    }
    return { url: location.href, title: document.title, items: list };
  }

  // ------------------------------------------------------------ links
  function links() {
    const out = [];
    const seen = new Set();
    for (const a of document.querySelectorAll('a[href]')) {
      if (!visible(a)) continue;
      const href = a.getAttribute('href');
      if (/^(javascript:|#)/.test(href)) continue;
      const url = abs(href);
      if (seen.has(url)) continue;
      seen.add(url);
      const t = text(a) || clean(a.getAttribute('aria-label') || a.title);
      if (!t) continue;
      out.push({ text: t.slice(0, 120), url, area: a.closest('nav, header') ? 'nav' : a.closest('footer') ? 'footer' : 'main' });
      if (out.length >= (args.max_links || 300)) break;
    }
    return { url: location.href, title: document.title, links: out };
  }

  // ------------------------------------------------------------ pagination
  const NEXT_WORDS = /^(next|next page|next ?[›»>→]|[›»→]|more results|show more results|older posts|older entries|load more|show more)$/i;
  function findNext() {
    const rel = document.querySelector('a[rel~="next"][href]');
    if (rel && visible(rel)) return rel;
    const cands = [...document.querySelectorAll('a, button, [role=button], [role=link]')].filter(visible);
    for (const el of cands) {
      if (el.disabled || el.getAttribute('aria-disabled') === 'true') continue;
      const label = clean(el.getAttribute('aria-label') || el.innerText || el.title);
      if (NEXT_WORDS.test(label) || (/\bnext\b/i.test(label) && !/prev/i.test(label) && label.length < 30)) return el;
    }
    // Numbered pagination: the link after the current page.
    const cur = document.querySelector('[aria-current="page"], .pagination .active, .pager .current');
    if (cur) {
      const li = cur.closest('li') || cur;
      const nxt = li.nextElementSibling;
      const a = nxt && (nxt.matches('a[href]') ? nxt : nxt.querySelector('a[href]'));
      if (a && visible(a)) return a;
    }
    const linkRel = document.querySelector('link[rel~="next"][href]');
    return linkRel || null;
  }

  function next() {
    const el = findNext();
    if (!el) return { found: false };
    const href = el.getAttribute('href');
    const navigable = href && !/^(javascript:|#)/.test(href);
    return { found: true, url: navigable ? abs(href) : '', label: clean(el.getAttribute('aria-label') || el.innerText || 'next').slice(0, 60) };
  }

  function clickNext() {
    const el = findNext();
    if (!el || el.tagName === 'LINK') return { clicked: false };
    el.scrollIntoView({ block: 'center' });
    el.click();
    return { clicked: true };
  }

  // ------------------------------------------------------------ watches
  function watch() {
    const kind = args.kind || 'price';
    let el = null;
    if (args.selector) {
      try { el = document.querySelector(args.selector); } catch (e) { return { error: 'bad selector: ' + e.message }; }
      if (!el) return { error: 'nothing on the page matches ' + args.selector };
    }
    if (kind === 'price') {
      if (el) {
        const m = text(el).match(PRICE);
        return m ? { value: clean(m[0]) } : { value: text(el).slice(0, 80) };
      }
      const ld = jsonLd();
      const p = ld.find((o) => types(o).some((t) => /^(Product|ProductGroup|Offer)$/.test(t)));
      const prod = p ? productFrom(p) : microdataProduct();
      if (prod && prod.price) return { value: (prod.currency ? prod.currency + ' ' : '') + prod.price, availability: prod.availability };
      const v = visiblePrices();
      return v.length ? { value: v[0] } : { error: 'no price found on the page' };
    }
    if (args.needle) {
      const hay = (el ? el.innerText : document.body.innerText) || '';
      return { value: hay.toLowerCase().includes(String(args.needle).toLowerCase()) ? 'present' : 'absent' };
    }
    return el ? { value: text(el).slice(0, 300) } : { error: 'a text watch needs a selector or text to look for' };
  }

  const modes = { summary, article, tables, items, links, next, click_next: clickNext, watch };
  if (!modes[mode]) return { error: 'unknown mode ' + mode };
  try { return modes[mode](); } catch (e) { return { error: String(e && e.message || e) }; }
})

use anyhow::Context;
use clap::Parser;
use oauth2::ClientId;
use ornithology_cli::{api, archive};
use rand::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

/// Twitter history introspection based on archive exports.
#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// Number of "top" items to show for each statistic.
    #[clap(short = 'n', long, default_value_t = 5)]
    top: u8,

    /// Ensure that fresh metrics are loaded for every tweet and user.
    ///
    /// This requires talking to the Twitter API, and can be _much_ slower especially if you have a
    /// lot of tweets or followers and start hitting Twitter's API rate limits:
    /// <https://developer.twitter.com/en/docs/twitter-api/rate-limits>.
    ///
    /// The first time you run this program, it _must_ load all the data, but on subsequent
    /// invocations it'll use cached data unless this flag is passed.
    #[clap(long)]
    fresh: bool,

    /// Path to your Twitter archive .zip file.
    ///
    /// To get this file, follow the instructions at
    /// <https://help.twitter.com/en/managing-your-account/how-to-download-your-twitter-archive>.
    /// It takes about 24h to get the archive after you submit the request, so come back later if
    /// you don't yet have said file :)
    archive: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let topn = args.top as usize;
    let archive = &*Box::leak(args.archive.into_boxed_path());

    let Loaded {
        me,
        old_rt_ids,
        mut tweets,
        mut followers,
    } = load(!args.fresh, archive).await.context("load dataset")?;

    let mut lists_of_tweets: HashMap<&'static str, Vec<String>> = HashMap::new();

    // It's fun to surface RTs that people may have forgotten about.
    if !old_rt_ids.is_empty() {
        // XXX: can theoretically look up tweet originals using
        // expansions=referenced_tweets.id and look for type=retweeted
        println!("remember these old retweets:");
        let mut rng = rand::thread_rng();
        let entry = lists_of_tweets
            .entry("old_rts")
            .or_insert_with(|| Vec::with_capacity(topn));
        for old_rt_id in old_rt_ids.choose_multiple(&mut rng, topn) {
            println!("https://twitter.com/{}/status/{}", me, old_rt_id);
            entry.push(old_rt_id.to_string());
        }
    }

    // Metrics that depend on the time-based state.
    // For example, how good was this tweet relative to other tweets at/up until that point in
    // time. The idea being that what makes a tweet "notable" isn't "does it have a lot of likes",
    // but "did it get a lot of likes relative to other tweets you twote back then"?
    {
        // For these, we need the tweet list to be sorted by time, so we use a block that sorts and
        // then borrows as read-only:
        tweets.sort_unstable_by_key(|t| t.created);
        let tweets = &tweets[..];
        fn find_notable(
            tweets: &[api::Tweet],
            mut metric: impl FnMut(&api::Tweet) -> usize,
            at_least: f64,
            at_least_x: f64,
        ) -> Vec<(f64, f64, usize)> {
            let mut avg = 0.0f64;
            let mut notable = Vec::new();
            for (i, t) in tweets.iter().enumerate() {
                let floor = (at_least_x * avg).max(at_least);
                let g = (metric)(t) as f64;
                if g > floor {
                    notable.push((g / avg, avg, i));
                }
                avg = 0.5 * g + 0.5 * avg;
            }
            notable.sort_unstable_by_key(|(x, _, i)| {
                // Reverse because we want the top ones to come first, not last.
                std::cmp::Reverse(((1000.0 * x).round() as usize, (metric)(&tweets[*i])))
            });
            notable
        }

        println!("notable tweets:");
        let notable = find_notable(&tweets, |t| t.goodness(), 10.0, 2.0);
        let entry = lists_of_tweets
            .entry("notable_tweets")
            .or_insert_with(|| Vec::with_capacity(topn));
        for (_, avg, i) in notable.into_iter().take(topn) {
            let tweet = &tweets[i];
            println!(
                "https://twitter.com/{}/status/{} ({} likes/{} rts when avg was {:.2})",
                me, tweet.id, tweet.metrics.likes, tweet.metrics.retweets, avg
            );
            entry.push(tweet.id.to_string());
        }

        println!("talked-about tweets:");
        let entry = lists_of_tweets
            .entry("talked_about_tweets")
            .or_insert_with(|| Vec::with_capacity(topn));
        let notable = find_notable(
            &tweets,
            |t| 2 * t.metrics.quotations + t.metrics.replies,
            10.0,
            2.0,
        );
        for (_, avg, i) in notable.into_iter().take(topn) {
            let tweet = &tweets[i];
            println!(
                "https://twitter.com/{}/status/{} ({} quotes + {} replies when avg as {:.2})",
                me, tweet.id, tweet.metrics.quotations, tweet.metrics.replies, avg
            );
            entry.push(tweet.id.to_string());
        }

        println!("over-shared tweets:");
        let entry = lists_of_tweets
            .entry("over_shared_tweets")
            .or_insert_with(|| Vec::with_capacity(topn));
        let notable = find_notable(
            &tweets,
            |t| 2 * t.metrics.quotations + t.metrics.retweets,
            10.0,
            2.0,
        );
        for (_, avg, i) in notable.into_iter().take(topn) {
            let tweet = &tweets[i];
            println!(
                "https://twitter.com/{}/status/{} ({} quotes + {} retweets when avg as {:.2})",
                me, tweet.id, tweet.metrics.quotations, tweet.metrics.retweets, avg
            );
            entry.push(tweet.id.to_string());
        }
    }

    // Now to the boring "best/most of all time" bits:
    println!("top tweets:");
    let entry = lists_of_tweets
        .entry("top_tweets")
        .or_insert_with(|| Vec::with_capacity(topn));
    tweets.sort_unstable_by_key(|t| t.goodness());
    for tweet in tweets.iter().rev().take(topn) {
        println!(
            "https://twitter.com/{}/status/{} ({} likes/{} rts)",
            me, tweet.id, tweet.metrics.likes, tweet.metrics.retweets
        );
        entry.push(tweet.id.to_string());
    }

    println!("most talked-about tweets:");
    let entry = lists_of_tweets
        .entry("most_talked_about_tweets")
        .or_insert_with(|| Vec::with_capacity(topn));
    tweets.sort_unstable_by_key(|t| 2 * t.metrics.quotations + t.metrics.replies);
    for tweet in tweets.iter().rev().take(topn) {
        println!(
            "https://twitter.com/{}/status/{} ({} quotes/{} replies)",
            me, tweet.id, tweet.metrics.quotations, tweet.metrics.replies
        );
        entry.push(tweet.id.to_string());
    }

    println!("most shared tweets:");
    let entry = lists_of_tweets
        .entry("most_shared_tweets")
        .or_insert_with(|| Vec::with_capacity(topn));
    tweets.sort_unstable_by_key(|t| 2 * t.metrics.quotations + t.metrics.retweets);
    for tweet in tweets.iter().rev().take(topn) {
        println!(
            "https://twitter.com/{}/status/{} ({} quotes/{} rts)",
            me, tweet.id, tweet.metrics.quotations, tweet.metrics.retweets
        );
        entry.push(tweet.id.to_string());
    }

    // Then we move on to follower stats.
    // First the obvious one:
    println!("top followers:");
    let entry = lists_of_tweets
        .entry("top_followers")
        .or_insert_with(|| Vec::with_capacity(topn));
    followers.sort_unstable_by_key(|f| f.metrics.followers);
    for follower in followers.iter().rev().take(topn) {
        println!(
            "https://twitter.com/{} ({} followers)",
            follower.username, follower.metrics.followers
        );
        entry.push(follower.username.to_string());
    }

    // What is a "neat" follower?
    // Well, we want to figure out who are "big" accounts that follow us, and where it is _notable_
    // that they do. For example, if an account with 1MM followers follow me, but it follows 1MM
    // other accounts, it's not that interesting (they probably just follow everyone back). But if
    // they follow _just_ me, that's very neat.
    println!("neat followers:");
    let entry = lists_of_tweets
        .entry("neat_followers")
        .or_insert_with(|| Vec::with_capacity(topn));
    followers
        .sort_unstable_by_key(|f| f.metrics.followers as isize - 10 * f.metrics.following as isize);
    for follower in followers.iter().rev().take(topn) {
        println!(
            "https://twitter.com/{} ({} followers but only following {})",
            follower.username, follower.metrics.followers, follower.metrics.following
        );
        entry.push(follower.username.to_string());
    }

    let groups = HashMap::from([
        ("top_tweets", "Top tweets"),
        ("most_talked_about_tweets", "Most talked about tweets"),
        ("most_shared_tweets", "Most shared tweets"),
        ("notable_tweets", "Notable tweets (at the time)"),
        ("talked_about_tweets", "Talked about tweets (at the time)"),
        ("over_shared_tweets", "Widely shared tweets (at the time)"),
        ("old_rts", "Random old retweets"),
    ]);
    for (id, _) in &groups {
        assert!(lists_of_tweets.contains_key(id), "{}", id);
    }
    let groups = serde_json::to_string(&groups).expect("serialize groups");

    let data = serde_json::to_string(&lists_of_tweets).expect("serialize lists_of_tweets");
    let html = format!(
        r#"
<!DOCTYPE html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <title>@{me} ornithology</title>
    <style>
      #tweets {{
        display: flex;
        flex-direction: row;
        flex-wrap: nowrap;
        gap: 0 2em;
      }}
      .list {{
        width: 30em;
        flex: none;
      }}
        .list h2 {{
          text-align: center;
          margin: 0;
          border: 1px solid rgb(207, 217, 222);
          border-radius: 12px;
          background: white;
          padding: 1em;
        }}
      #followers {{
        list-style-type: none;
        padding: 0;
      }}
        #followers li {{
          display: block;
          margin: 1em;
          border: 1px solid rgb(207, 217, 222);
          border-radius: 12px;
          background: white;
          padding: 1em;
        }}
          #followers li strong {{
            margin-right: .5em;
          }}
          #followers li a + a::before {{
            content: ",";
            margin: 0 0.5ex;
          }}
    </style>
  </head>
  <body>
    <ul id="followers"></ul>
    <div id="tweets"></div>
  <script charset="utf-8">
    var data = {data};

    var followers = document.getElementById('followers');
    [['top_followers', 'Top followers'], ['neat_followers', 'Neat followers']].forEach(([id, title]) => {{
      var li = document.createElement('li');
      var s = document.createElement('strong');
      s.textContent = title + ':';
      li.appendChild(s);
      for (var i in data[id]) {{
        var u = data[id][i];
        var a = document.createElement('a');
        a.setAttribute('href', 'https://twitter.com/' + u);
        a.innerText = '@' + u;
        li.appendChild(a);
      }}
      followers.appendChild(li);
    }});

    var groups = {groups};
    var tweets = document.getElementById('tweets');
    Object.entries(groups).forEach(([id, title]) => {{
      var d = document.createElement('div');
      d.id = id;
      d.classList.add("list");
      var t = document.createElement('h2');
      t.innerText = title;
      d.appendChild(t);
      tweets.appendChild(d);
    }});
  </script>
  <script src="https://platform.twitter.com/widgets.js" charset="utf-8"></script>
  <script charset="utf-8">
    Object.keys(groups).forEach(group => {{
        var el = document.getElementById(group);
        data[group].forEach(id => {{
          twttr.widgets.createTweet(id, el);
        }})
    }});
  </script>
  </body>
</html>
"#,
    );
    let f = Path::new("ornithology.html");
    tokio::fs::write(&f, &html)
        .await
        .context("write ornithology.html")?;
    open::that(f).context("open generated page")?;

    // TODO: add plots, like scatter plot of time/likes (prob. include id for easy reference)

    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct Loaded {
    me: String,
    old_rt_ids: Vec<u64>,
    tweets: Vec<api::Tweet>,
    followers: Vec<api::User>,
}

async fn load(use_cache: bool, archive: &'static Path) -> anyhow::Result<Loaded> {
    let cache_file = Path::new("cache.json");
    if use_cache && cache_file.exists() {
        let s = tokio::fs::read(&cache_file)
            .await
            .with_context(|| format!("read {}", cache_file.display()))?;
        return Ok(serde_json::from_slice(&s)
            .with_context(|| format!("parse {}", cache_file.display()))?);
    }

    let (old_rt_ids, follower_ids, tweet_ids) = tokio::task::spawn_blocking(|| {
        let fname = Path::new(archive);
        let zipfile = std::fs::File::open(&fname).context("open twitter archive")?;
        let mut archive = zip::ZipArchive::new(zipfile).context("open twitter archive as zip")?;

        let followers: Vec<u64> = archive::parse(
            &mut archive,
            "data/follower.js",
            |archive::Follower::One { id }| Some(id),
        )
        .context("extract follower list")?;

        let mut oldies = Vec::new();
        let tweets: Vec<u64> = archive::parse(
            &mut archive,
            "data/tweet.js",
            |archive::Tweet::One { id, text }| {
                if text.starts_with("RT @") {
                    oldies.push(id);
                    None
                } else {
                    Some(id)
                }
            },
        )
        .context("extract follower list")?;

        Ok::<_, anyhow::Error>((oldies, followers, tweets))
    })
    .await
    .context("spawn blocking")??;

    let client_id = ClientId::new("SUtlNTYydEhnVDJEOW5uSmh3Q0g6MTpjaQ".to_string());
    let mut client = api::Client::new(client_id)
        .await
        .context("api::Client::new")?;

    // Let's first figure out which user we are
    let whoami = client.whoami().await.context("whoami")?;
    eprintln!("whoami: @{} ({})", whoami.username, whoami.id);

    // Now get stats about each tweet:
    let tweets = client.tweets(tweet_ids).await.context("fetch tweets")?;

    // and about each follower:
    let followers = client.users(follower_ids).await.context("fetch follower")?;

    let loaded = Loaded {
        me: whoami.username,
        old_rt_ids,
        tweets,
        followers,
    };
    tokio::fs::write(
        &cache_file,
        &serde_json::to_vec(&loaded).context("serialize cache.json")?,
    )
    .await
    .with_context(|| format!("write {}", cache_file.display()))?;

    Ok(loaded)
}

#import "prelude.typ": conf

#show: conf.with(mode: "shiyu", size: "big")

#let input = json("input.json");

#align(center)[
  #image("images/shiyu.webp", width: 25%)
  #input.name, id: #input.id\
  #input.begin_time.split(" ").first() -
  #input.end_time.split(" ").first()
]


#for (k, (id, room)) in input.zone.pairs().sorted(key: it => it.at(1).name).enumerate() {
  if room.layer_room.len() == 0 { continue }
  if "Critical" in room.name { continue }
  rect(width: 100%, radius: 10pt)[
    #set text(size: 8pt)
    #text(size: 11pt, baseline: 2pt)[#room.name]
    #let elements = room.layer_room.values().first().monster_list.values().first().element
    #if elements.values().contains(-1) or elements.values().contains(1) [
      #columns(2)[
        #if elements.filter(x => x == 1).len() != 0 [
          Weakness:
          #for element in elements.filter(x => x == 1).keys().intersperse(", ") [
            #element
          ]
        ]
        #colbreak()
        #if elements.filter(x => x == -1).len() != 0 [
          Resistance:
          #for element in elements.filter(x => x == -1).keys().intersperse(", ") [
            #element
          ]
        ]
      ]
    ]

    #grid(columns: (1fr, 1fr), inset: 4pt, ..room
        .layer_room
        .values()
        .first()
        .monster_list
        .values()
        .sorted(key: x => x.stats.at("hp"))
        .rev()
        .map(monster => [
          #rect(width: 100%)[
            #monster.name\
            #let stats = monster.stats
            #let elements = monster.element
            #if elements.filter(x => x == 1).len() != 0 [
              Weakness:
              #for element in elements.filter(x => x == 1).keys().intersperse(", ") [
                #element
              ]
            ]
            #h(0.25cm)
            #if elements.filter(x => x == -1).len() != 0 [
              Resistance:
              #for element in elements.filter(x => x == -1).keys().intersperse(", ") [
                #element
              ]
            ]


            HP: #int(stats.at("hp"))\
            ATK: #int(stats.at("attack"))\
            DEF: #int(stats.at("defence"))\
          ]
        ]))


    #for buff in room.layer_buff.values().filter(x => x.desc.len() != 0) [
      #let lines = buff.desc.replace("\n", "").replace("·", "\n·").trim()
      #lines\
    ]

  ]
}
